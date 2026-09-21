//! Regressions for defects an adversarial review found.
//!
//! Each test names the behaviour that was wrong and pins down the behaviour that
//! replaced it, so a change that reintroduces the defect fails here rather than in a
//! fuzz run months later.

use std::ops::Bound;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier};

use drydb::{
    AsciiEncoding, Database, DatabaseBuilder, ErrorKind, Int64Encoding, MemorySource, OpenOptions,
    Order, PageSource, UlidEncoding, VerifyOptions,
};

fn build_i64(rows: i64, page_size: usize, value_len: usize) -> Vec<u8> {
    let mut builder = DatabaseBuilder::new().page_size(page_size).unwrap();
    let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    for i in 0..rows {
        let value: Vec<u8> = (0..value_len)
            .map(|n| (i as u8).wrapping_add(n as u8))
            .collect();
        builder
            .append(table, &Int64Encoding::encode(i), &value)
            .unwrap();
    }
    builder.build_to_vec().unwrap()
}

/// Offset of a page in the file, read out of the page directory.
fn page_offset(bytes: &[u8], ordinal: u64) -> usize {
    let directory = i64::from_le_bytes(bytes[18..26].try_into().unwrap()) as usize;
    let slot = directory + ordinal as usize * 8;
    i64::from_le_bytes(bytes[slot..slot + 8].try_into().unwrap()) as usize
}

/// A page source that counts reads and can be told to refuse them.
struct WatchedSource {
    inner: MemorySource,
    reads: AtomicU64,
    forbidden: AtomicBool,
}

impl PageSource for WatchedSource {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        if self.forbidden.load(Ordering::Relaxed) {
            return Err(std::io::Error::other("read while reads were forbidden"));
        }
        self.inner.read_at(buf, offset)
    }

    fn size(&self) -> std::io::Result<u64> {
        self.inner.size()
    }
}

/// Streaming a blob used to trust the length in the page header, so a page claiming to
/// run to the end of the file handed back the pages after it, page directory included.
#[test]
fn a_blob_page_cannot_read_past_the_page_directory() {
    let mut builder = DatabaseBuilder::new().page_size(256).unwrap();
    let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    builder
        .append(table, &Int64Encoding::encode(1), &vec![0xA5u8; 256])
        .unwrap();
    builder
        .append(table, &Int64Encoding::encode(2), &vec![0x5Au8; 256])
        .unwrap();
    let mut bytes = builder.build_to_vec().unwrap();

    // The first blob page is ordinal 0: it is written before the leaf that points at it.
    let blob = page_offset(&bytes, 0);
    let directory = i64::from_le_bytes(bytes[18..26].try_into().unwrap()) as usize;
    let overreach = (bytes.len() - blob) as i32;
    bytes[blob..blob + 4].copy_from_slice(&overreach.to_le_bytes());
    assert!(blob + overreach as usize > directory);

    let db = Database::open_bytes(bytes).unwrap();
    let table = db.table("t").unwrap();
    // Reading it as a value already refused; streaming has to refuse it too.
    assert!(table.get(&Int64Encoding::encode(1)).is_err());
    let err = table.blob_reader(&Int64Encoding::encode(1)).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::CorruptData);
    assert!(err.to_string().contains("into what follows it"), "{err}");
}

/// `get_cached` promises never to perform I/O. It used to re-ask the store for a blob
/// page it had already found in the cache, which reads when the page was evicted in
/// between.
#[test]
fn get_cached_never_reads_the_file() {
    let mut builder = DatabaseBuilder::new().page_size(256).unwrap();
    let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    for i in 0..4i64 {
        builder
            .append(table, &Int64Encoding::encode(i), &vec![i as u8; 256])
            .unwrap();
    }
    let source = Arc::new(WatchedSource {
        inner: MemorySource::new(builder.build_to_vec().unwrap()),
        reads: AtomicU64::new(0),
        forbidden: AtomicBool::new(false),
    });
    let db = Arc::new(
        OpenOptions::new()
            .memory_budget(1 << 20)
            // Small enough that the churning thread evicts constantly.
            .cache_capacity(1024)
            .cache_shards(1)
            .open_source(Arc::clone(&source) as Arc<dyn PageSource>)
            .unwrap(),
    );

    let stop = Arc::new(AtomicBool::new(false));
    let barrier = Arc::new(Barrier::new(2));
    std::thread::scope(|scope| {
        {
            let db = Arc::clone(&db);
            let stop = Arc::clone(&stop);
            let barrier = Arc::clone(&barrier);
            scope.spawn(move || {
                let table = db.table("t").unwrap();
                barrier.wait();
                while !stop.load(Ordering::Relaxed) {
                    for i in 0..4i64 {
                        let _ = table.get(&Int64Encoding::encode(i));
                    }
                }
            });
        }

        let table = db.table("t").unwrap();
        barrier.wait();
        for _ in 0..3000 {
            for i in 0..4i64 {
                // Any read on this thread turns into an error the lookup would surface.
                source.forbidden.store(true, Ordering::Relaxed);
                let outcome = table.get_cached(&Int64Encoding::encode(i));
                source.forbidden.store(false, Ordering::Relaxed);
                assert!(
                    outcome.is_ok(),
                    "get_cached performed I/O: {:?}",
                    outcome.err().map(|e| e.to_string())
                );
            }
        }
        stop.store(true, Ordering::Relaxed);
    });
}

/// A digest that disagrees with its key makes a search start past the row it is looking
/// for. The result was quietly short; now the page is rejected when it is read.
#[test]
fn a_page_whose_digests_disagree_with_its_keys_is_rejected() {
    let mut builder = DatabaseBuilder::new().page_size(4096).unwrap();
    let table = builder.create_table("t", Arc::new(AsciiEncoding)).unwrap();
    for key in ["alpha", "bravo", "charlie"] {
        builder
            .append(table, key.as_bytes(), key.as_bytes())
            .unwrap();
    }
    let mut bytes = builder.build_to_vec().unwrap();

    let db = Database::open_bytes(bytes.clone()).unwrap();
    let root = db.catalog().tables[0].primary.root.unwrap().get();
    drop(db);

    // Zero the digest array of the root leaf, leaving every other field intact.
    let page = page_offset(&bytes, root);
    for slot in 0..3 {
        let at = page + 28 + slot * 8;
        bytes[at..at + 8].copy_from_slice(&0u64.to_le_bytes());
    }

    let db = Database::open_bytes(bytes.clone()).unwrap();
    let table = db.table("t").unwrap();
    let err = table.get(b"alpha").unwrap_err();
    assert_eq!(err.kind(), ErrorKind::CorruptData);
    assert!(err.to_string().contains("digest"), "{err}");

    // With the check turned off the search runs on the file as it is, which is what the
    // option documents; it must still not be a panic.
    let db = OpenOptions::new()
        .validate_digests(false)
        .open_bytes(bytes)
        .unwrap();
    let table = db.table("t").unwrap();
    let _ = table.get(b"alpha");
    let _ = table.count();
}

/// The catalog used to be read and parsed before the budget existed, so a database with
/// a megabyte of table names opened inside a sixteen kilobyte budget.
#[test]
fn the_catalog_is_charged_to_the_memory_budget() {
    let mut builder = DatabaseBuilder::new().page_size(128).unwrap();
    for i in 0..40 {
        let name = format!("{}{i:03}", "n".repeat(2000));
        let table = builder.create_table(name, Arc::new(Int64Encoding)).unwrap();
        builder
            .append(table, &Int64Encoding::encode(1), b"v")
            .unwrap();
    }
    let bytes = builder.build_to_vec().unwrap();
    assert!(bytes.len() > 80_000);

    let err = OpenOptions::new()
        .memory_budget(16 * 1024)
        .open_bytes(bytes.clone())
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::BudgetExceeded);

    let db = OpenOptions::new()
        .memory_budget(4 << 20)
        .open_bytes(bytes)
        .unwrap();
    assert!(
        db.memory_report().charged > 80_000,
        "the catalog should be charged, saw {}",
        db.memory_report().charged
    );
}

/// The page directory reserved memory for a new chunk before releasing an old one, so a
/// scan stopped partway through once the resident chunks filled a tight budget.
#[test]
fn a_scan_completes_with_a_tight_budget_and_several_directory_chunks() {
    let bytes = build_i64(20_000, 128, 8);
    let db = OpenOptions::new()
        .memory_budget(16 * 1024)
        .cache_capacity(0)
        .cache_shards(1)
        .directory_chunks(8)
        .open_bytes(bytes)
        .unwrap();
    let table = db.table("t").unwrap();

    let mut cursor = table.scan(Order::Ascending).unwrap();
    let mut seen = 0i64;
    while cursor
        .advance()
        .expect("the scan must not run out of budget")
    {
        seen += 1;
    }
    assert_eq!(seen, 20_000);

    // Verification walks the same pages with the same budget.
    let report = db.verify(Default::default()).unwrap();
    assert!(report.is_ok(), "{:?}", report.problems);
    assert!(db.memory_report().charged <= 16 * 1024);
}

/// A prefix is shorter than a key, so validating it as one rejected every prefix of a
/// fixed-width encoding.
#[test]
fn prefix_queries_work_on_fixed_width_byte_ordered_keys() {
    let mut builder = DatabaseBuilder::new().page_size(512).unwrap();
    let table = builder.create_table("t", Arc::new(UlidEncoding)).unwrap();
    let mut keys = Vec::new();
    for i in 0..16u8 {
        let mut key = [0u8; 16];
        key[0] = i / 4;
        key[1] = i;
        keys.push(key);
        builder.append(table, &key, &[i]).unwrap();
    }
    let bytes = builder.build_to_vec().unwrap();

    let db = Database::open_bytes(bytes).unwrap();
    let table = db.table("t").unwrap();

    let mut cursor = table.prefix(&[1u8], Order::Ascending).unwrap();
    let mut found = Vec::new();
    while cursor.advance().unwrap() {
        found.push(cursor.current().unwrap().key().to_vec());
    }
    let expected: Vec<Vec<u8>> = keys
        .iter()
        .filter(|k| k[0] == 1)
        .map(|k| k.to_vec())
        .collect();
    assert_eq!(found, expected);
    assert_eq!(found.len(), 4);

    // A full-width prefix still selects exactly its key.
    let mut cursor = table.prefix(&keys[7], Order::Ascending).unwrap();
    assert!(cursor.advance().unwrap());
    assert_eq!(cursor.current().unwrap().key(), &keys[7]);
    assert!(!cursor.advance().unwrap());

    // And a key of the wrong width is still refused where a key is expected.
    assert_eq!(
        table.get(&[1u8]).unwrap_err().kind(),
        ErrorKind::InvalidArgument
    );
}

/// Counting a range also has to survive the tight-budget directory case.
#[test]
fn counting_completes_with_a_tight_budget() {
    let bytes = build_i64(20_000, 128, 8);
    let db = OpenOptions::new()
        .memory_budget(16 * 1024)
        .cache_capacity(0)
        .directory_chunks(8)
        .open_bytes(bytes)
        .unwrap();
    let table = db.table("t").unwrap();
    assert_eq!(table.count().unwrap(), 20_000);
    assert_eq!(
        table
            .count_range(
                Bound::Included(&Int64Encoding::encode(5_000)[..]),
                Bound::Excluded(&Int64Encoding::encode(15_000)[..]),
            )
            .unwrap(),
        10_000
    );
}

/// A key encoding whose digest keeps only the leading `i64`, so the four record-id bytes
/// after it are outside the digest. That is exactly the shape a non-unique secondary
/// index has upstream: many distinct keys share one digest.
///
/// `exact` is what the encoding claims about its digest. The builder writes pages with
/// the key bytes omitted when it is told the digest is injective, which is the state
/// upstream's own builder puts non-unique index pages in, and a reader that knows better
/// then has nothing but the digest to compare.
#[derive(Debug)]
struct CoarseKey {
    exact: bool,
}

impl drydb::KeyEncoding for CoarseKey {
    fn id(&self) -> &str {
        "coarse-i64"
    }

    fn compare(&self, a: &[u8], b: &[u8]) -> Result<std::cmp::Ordering, drydb::Error> {
        let source = Int64Encoding.compare(&a[..8], &b[..8])?;
        Ok(source.then_with(|| a[8..].cmp(&b[8..])))
    }

    fn digest(&self, key: &[u8]) -> Result<u64, drydb::Error> {
        Int64Encoding.digest(&key[..8])
    }

    fn is_digest_exact(&self) -> bool {
        self.exact
    }

    fn decode_key_from_digest(&self, digest: u64, out: &mut Vec<u8>) -> Result<(), drydb::Error> {
        Int64Encoding.decode_key_from_digest(digest, out)?;
        out.extend_from_slice(&0i32.to_le_bytes());
        Ok(())
    }

    fn fixed_key_len(&self) -> Option<usize> {
        Some(12)
    }

    fn max_rebuilt_key_len(&self) -> Option<usize> {
        Some(12)
    }
}

fn coarse_key(source: i64, rid: i32) -> Vec<u8> {
    let mut key = Int64Encoding::encode(source).to_vec();
    key.extend_from_slice(&rid.to_le_bytes());
    key
}

/// Descending into the first child whose separator compares equal lost every matching row
/// that started in the child before it. On a page with the keys omitted an equal
/// comparison is only an equal digest, so the previous child can end with rows that
/// match, and a rightward walk never reaches a child already passed.
#[test]
fn rows_that_start_before_the_matching_separator_are_still_found() {
    let mut builder = DatabaseBuilder::new().page_size(128).unwrap();
    let table = builder
        .create_table("t", Arc::new(CoarseKey { exact: true }))
        .unwrap();
    // Five rows under source key 0, thirty-five under source key 1. The boundary lands
    // inside the first leaf, so the separator for the second child also digests to 1.
    for rid in 0..40i32 {
        let source = if rid < 5 { 0 } else { 1 };
        builder
            .append(table, &coarse_key(source, rid), &[rid as u8])
            .unwrap();
    }
    let bytes = builder.build_to_vec().unwrap();
    let bytes_ref = bytes.clone();

    let db = OpenOptions::new()
        .register_encoding(Arc::new(CoarseKey { exact: false }))
        .unwrap()
        .open_bytes(bytes)
        .unwrap();
    let root = db.catalog().tables[0].primary.root.unwrap().get();
    let table = db.table("t").unwrap();

    // The repro only means anything if the root really is an internal page whose
    // separators carry nothing but a digest. (Rebuilt keys all collapse to `source || 0`
    // on such a page, which is why `verify` is not used here: it reports the rows that
    // upstream's own non-unique index pages lose in exactly the same way.)
    let kind_word = i32::from_le_bytes(
        bytes_ref[page_offset(&bytes_ref, root) + 4..][..4]
            .try_into()
            .unwrap(),
    );
    assert_eq!(kind_word & 0xFF, 1, "the root has to be an internal page");
    assert_ne!(kind_word & (1 << 11), 0, "the pages have to omit key bytes");

    let lower = Bound::Included(coarse_key(1, 0));
    let upper = Bound::Excluded(coarse_key(2, 0));
    let mut cursor = table
        .range(
            lower.as_ref().map(Vec::as_slice),
            upper.as_ref().map(Vec::as_slice),
            Order::Ascending,
        )
        .unwrap();
    let mut seen = 0usize;
    while cursor.advance().unwrap() {
        seen += 1;
    }
    assert_eq!(seen, 35, "every row under source key 1 has to be reachable");

    assert_eq!(
        table
            .count_range(
                lower.as_ref().map(Vec::as_slice),
                upper.as_ref().map(Vec::as_slice)
            )
            .unwrap(),
        35
    );

    // The exact search walks the same way: the first row for source key 1 is not on the
    // leaf the descent reaches.
    assert!(table.get(&coarse_key(1, 0)).unwrap().is_some());
    assert!(table.contains_key(&coarse_key(1, 0)).unwrap());
    // And a key that is genuinely absent still reports absent.
    assert!(table.get(&coarse_key(7, 0)).unwrap().is_none());
}

/// Digest validation used to run inside the page load, so any other reader of the same
/// file could fill the cache first and leave the tree searching unchecked bytes.
#[test]
fn digests_are_checked_even_when_something_else_cached_the_page_first() {
    let mut builder = DatabaseBuilder::new().page_size(4096).unwrap();
    let table = builder.create_table("t", Arc::new(AsciiEncoding)).unwrap();
    for key in ["alpha", "bravo", "charlie"] {
        builder
            .append(table, key.as_bytes(), key.as_bytes())
            .unwrap();
    }
    let mut bytes = builder.build_to_vec().unwrap();

    let db = Database::open_bytes(bytes.clone()).unwrap();
    let root = db.catalog().tables[0].primary.root.unwrap().get();
    drop(db);

    let page = page_offset(&bytes, root);
    for slot in 0..3 {
        let at = page + 28 + slot * 8;
        bytes[at..at + 8].copy_from_slice(&0u64.to_le_bytes());
    }

    let db = Database::open_bytes(bytes).unwrap();
    // `verify` sweeps every page into the cache without knowing the key encoding.
    let report = db.verify(Default::default()).unwrap();
    assert!(!report.is_ok(), "verification has to report the page");

    // The page is cached now. The search must still refuse it.
    let table = db.table("t").unwrap();
    let err = table.get(b"alpha").unwrap_err();
    assert_eq!(err.kind(), ErrorKind::CorruptData);
    assert!(err.to_string().contains("digest"), "{err}");
}

/// Eytzinger pages were exempt from digest validation although they do store keys, so
/// the defect the check exists for survived on exactly the layout that is hardest to
/// reason about.
#[test]
fn eytzinger_digests_are_checked_against_their_keys() {
    let mut builder = DatabaseBuilder::new()
        .page_size(4096)
        .unwrap()
        .eytzinger_digests(true);
    let table = builder.create_table("t", Arc::new(AsciiEncoding)).unwrap();
    for key in ["alpha", "bravo", "charlie", "delta", "echo"] {
        builder
            .append(table, key.as_bytes(), key.as_bytes())
            .unwrap();
    }
    let mut bytes = builder.build_to_vec().unwrap();

    let db = Database::open_bytes(bytes.clone()).unwrap();
    assert!(db.verify(Default::default()).unwrap().is_ok());
    assert!(db.table("t").unwrap().get(b"delta").unwrap().is_some());
    let root = db.catalog().tables[0].primary.root.unwrap().get();
    drop(db);

    // Swap two slots of the padded tree. The page stays structurally valid and the
    // digests stay sorted in their own order; only the pairing with the keys breaks.
    let page = page_offset(&bytes, root);
    let a = page + 28;
    let b = page + 28 + 8;
    for i in 0..8 {
        bytes.swap(a + i, b + i);
    }

    let db = Database::open_bytes(bytes.clone()).unwrap();
    let err = db.table("t").unwrap().get(b"delta").unwrap_err();
    assert_eq!(err.kind(), ErrorKind::CorruptData);
    assert!(err.to_string().contains("digest"), "{err}");

    // The padding slots are load-bearing too: the descent treats them as larger than
    // every key, so a slot that is not the sentinel sends it down the wrong branch.
    let mut padded = bytes.clone();
    let slots = 7usize; // complete_size(5)
    let last = page + 28 + (slots - 1) * 8;
    padded[last..last + 8].copy_from_slice(&0u64.to_le_bytes());
    let db = Database::open_bytes(padded).unwrap();
    let err = db.table("t").unwrap().get(b"delta").unwrap_err();
    assert_eq!(err.kind(), ErrorKind::CorruptData);
}

/// The catalog's reservation used to be released with the `Database`, while the table and
/// index handles it produced kept their own copies of the descriptors alive. Dropping the
/// database therefore freed budget that nothing had actually stopped using, and the next
/// read could exceed the limit for real.
#[test]
fn handles_keep_the_catalog_charged_after_the_database_is_dropped() {
    // Long table names, so the catalog is a visible share of the budget, and one value
    // large enough that it only fits if the catalog's share has been released.
    let mut builder = DatabaseBuilder::new().page_size(4096).unwrap();
    let mut names = Vec::new();
    for i in 0..30 {
        let name = format!("{}{i:02}", "t".repeat(2000));
        let table = builder
            .create_table(name.clone(), Arc::new(Int64Encoding))
            .unwrap();
        names.push(name);
        builder
            .append(table, &Int64Encoding::encode(0), &vec![b'v'; 750_000])
            .unwrap();
    }
    let bytes = builder.build_to_vec().unwrap();

    let db = OpenOptions::new()
        .memory_budget(800_000)
        .open_bytes(bytes)
        .unwrap();
    let charged = db.memory_report().charged;
    assert!(
        charged > 0,
        "the catalog has to be charged while the database is open"
    );

    let table = db.table(&names[0]).unwrap();
    let key = Int64Encoding::encode(0);
    let with_database = table.get(&key).unwrap_err();
    assert_eq!(with_database.kind(), ErrorKind::BudgetExceeded);

    // The handle still holds copies of the descriptors, so the same read has to fail the
    // same way once the database itself is gone.
    drop(db);
    let without_database = table.get(&key).unwrap_err();
    assert_eq!(
        without_database.kind(),
        ErrorKind::BudgetExceeded,
        "dropping the database released memory the handle is still using"
    );
}

/// Builds a file whose primary tree has several internal pages, and returns it with the
/// ordinal of an internal page that is not on the leftmost root-to-leaf path.
fn tree_with_several_internal_pages(eytzinger: bool) -> (Vec<u8>, u64) {
    let mut builder = DatabaseBuilder::new()
        .page_size(256)
        .unwrap()
        .eytzinger_digests(eytzinger);
    let table = builder.create_table("t", Arc::new(AsciiEncoding)).unwrap();
    for i in 0..400 {
        let key = format!("{i:08}");
        builder
            .append(table, key.as_bytes(), key.as_bytes())
            .unwrap();
    }
    let bytes = builder.build_to_vec().unwrap();

    let db = Database::open_bytes(bytes.clone()).unwrap();
    let root = db.catalog().tables[0].primary.root.unwrap().get();
    drop(db);

    // The second child of the root: an internal page no search for a small key reaches.
    let page = page_offset(&bytes, root);
    let kind_word = i32::from_le_bytes(bytes[page + 4..page + 8].try_into().unwrap());
    assert_eq!(kind_word & 0xFF, 1, "the root has to be an internal page");
    let entries = entry_count(&bytes, page);
    assert!(entries >= 2, "the root has to have at least two children");
    // Compact metadata: one u16 entry-end offset per entry plus a leading start offset.
    // The child ordinal is the last eight bytes of an entry's payload.
    let meta = page + 28 + digest_slots(&bytes, page) * 8;
    let end = u16::from_le_bytes(bytes[meta + 4..meta + 6].try_into().unwrap()) as usize;
    let child = i64::from_le_bytes(bytes[page + end - 8..page + end].try_into().unwrap());
    (bytes, child as u64)
}

fn entry_count(bytes: &[u8], page: usize) -> usize {
    i32::from_le_bytes(bytes[page + 8..page + 12].try_into().unwrap()) as usize
}

/// Number of `u64` slots in a page's digest area.
fn digest_slots(bytes: &[u8], page: usize) -> usize {
    let kind_word = i32::from_le_bytes(bytes[page + 4..page + 8].try_into().unwrap());
    let entries = entry_count(bytes, page);
    if kind_word & (1 << 9) != 0 {
        // A complete binary tree padded to the next power of two, minus one.
        (entries + 1).next_power_of_two() - 1
    } else {
        entries
    }
}

/// Verification walked the leaf sibling chain and nothing else, so an internal page that
/// no leaf walk touches could be corrupt while `verify` reported the file clean. A search
/// that descends into that page fails, which is the case verification exists to find.
#[test]
fn verification_reaches_internal_pages_off_the_leftmost_path() {
    for eytzinger in [false, true] {
        let (mut bytes, child) = tree_with_several_internal_pages(eytzinger);

        let db = Database::open_bytes(bytes.clone()).unwrap();
        assert!(
            db.verify(Default::default()).unwrap().is_ok(),
            "the fixture has to start clean (eytzinger = {eytzinger})"
        );
        drop(db);

        // Zero that page's digest array, leaving everything else intact.
        let page = page_offset(&bytes, child);
        for slot in 0..digest_slots(&bytes, page) {
            let at = page + 28 + slot * 8;
            bytes[at..at + 8].copy_from_slice(&0u64.to_le_bytes());
        }

        let db = Database::open_bytes(bytes).unwrap();
        let report = db.verify(Default::default()).unwrap();
        assert!(
            !report.is_ok(),
            "verification missed the damaged internal page (eytzinger = {eytzinger})"
        );
        assert!(
            report
                .problems
                .iter()
                .any(|p| p.page == Some(child) && p.message.contains("digest")),
            "expected a digest problem on page {child}, got {:?}",
            report.problems
        );
    }
}

/// Verification checked each internal page it read but not the children those pages point
/// at, so a file whose separator names a page that does not exist was reported clean while
/// a search that descended into it failed outright.
#[test]
fn verification_reports_a_child_that_is_not_a_page_of_the_file() {
    let (mut bytes, child) = tree_with_several_internal_pages(false);
    let page = page_offset(&bytes, child);
    assert!(entry_count(&bytes, page) >= 1);
    // Point the page's first child at an ordinal far past the end of the file.
    let at = internal_child_at(&bytes, page, 0);
    bytes[at..at + 8].copy_from_slice(&999_999i64.to_le_bytes());

    let db = Database::open_bytes(bytes).unwrap();
    let report = db.verify(Default::default()).unwrap();
    assert!(
        report
            .problems
            .iter()
            .any(|p| p.page == Some(child) && p.message.contains("999999")),
        "expected the out of range child to be reported, got {:?}",
        report.problems
    );
}

/// A level whose sibling chain stops early leaves pages that no walk reaches. Counting
/// the children the level above declares against the pages the chain actually visits
/// catches it without holding more than one page.
#[test]
fn verification_reports_a_level_whose_sibling_chain_stops_early() {
    let (mut bytes, child) = tree_with_several_internal_pages(false);
    // Cut the chain at the page after the root's second child.
    let page = page_offset(&bytes, child);
    bytes[page + 20..page + 28].copy_from_slice(&(-1i64).to_le_bytes());

    let db = Database::open_bytes(bytes).unwrap();
    let report = db.verify(Default::default()).unwrap();
    assert!(
        report
            .problems
            .iter()
            .any(|p| p.message.contains("level below") || p.message.contains("level above")),
        "expected the truncated chain to be reported, got {:?}",
        report.problems
    );
}

/// Start and length of the metadata area of a compact page.
fn meta_base(bytes: &[u8], page: usize) -> usize {
    page + 28 + digest_slots(bytes, page) * 8
}

/// Byte offset of the child ordinal of one internal entry.
///
/// A page that omits key bytes stores a dense array of ordinals right after the digests;
/// otherwise the compact metadata gives each entry's end and the ordinal is its last
/// eight bytes.
fn internal_child_at(bytes: &[u8], page: usize, index: usize) -> usize {
    let kind_word = i32::from_le_bytes(bytes[page + 4..page + 8].try_into().unwrap());
    let meta = meta_base(bytes, page);
    if kind_word & (1 << 11) != 0 {
        return meta + index * 8;
    }
    let end = u16::from_le_bytes(
        bytes[meta + (index + 1) * 2..meta + (index + 1) * 2 + 2]
            .try_into()
            .unwrap(),
    ) as usize;
    page + end - 8
}

fn internal_child(bytes: &[u8], page: usize, index: usize) -> u64 {
    let at = internal_child_at(bytes, page, index);
    i64::from_le_bytes(bytes[at..at + 8].try_into().unwrap()) as u64
}

/// Where one leaf entry's payload starts, and how long its key is.
fn leaf_entry_span(bytes: &[u8], page: usize, index: usize) -> (usize, usize) {
    let entries = entry_count(bytes, page);
    let meta = meta_base(bytes, page);
    let start = u16::from_le_bytes(
        bytes[meta + index * 2..meta + index * 2 + 2]
            .try_into()
            .unwrap(),
    ) as usize
        & 0x7FFF;
    let key_len_at = meta + (entries + 1) * 2 + index * 2;
    let key_len =
        u16::from_le_bytes(bytes[key_len_at..key_len_at + 2].try_into().unwrap()) as usize;
    (page + start, key_len)
}

/// Searching a leaf trusts that its keys ascend: it starts where the digests say and
/// stops at the first key above the one it wants. Keys that share a digest can be out of
/// order among themselves while every digest still matches its own key, and the search
/// then reported a row that is on the page as absent. Page order is now checked, so the
/// file is refused instead.
#[test]
fn a_leaf_whose_keys_are_out_of_order_is_rejected() {
    let mut builder = DatabaseBuilder::new().page_size(256).unwrap();
    let table = builder.create_table("t", Arc::new(AsciiEncoding)).unwrap();
    // The digest of an `ascii` key covers its first eight bytes, so all three share one.
    for key in ["abcdefgh0", "abcdefgh1", "abcdefgh2"] {
        builder
            .append(table, key.as_bytes(), format!("v{key}").as_bytes())
            .unwrap();
    }
    let mut bytes = builder.build_to_vec().unwrap();

    let db = Database::open_bytes(bytes.clone()).unwrap();
    assert!(db.verify(Default::default()).unwrap().is_ok());
    assert!(db.table("t").unwrap().get(b"abcdefgh0").unwrap().is_some());
    let root = db.catalog().tables[0].primary.root.unwrap().get();
    drop(db);

    // Swap the last byte of the first two keys. Their digests still match their keys.
    let page = page_offset(&bytes, root);
    let (first, first_len) = leaf_entry_span(&bytes, page, 0);
    let (second, _) = leaf_entry_span(&bytes, page, 1);
    assert_eq!(&bytes[first..first + first_len], b"abcdefgh0");
    bytes.swap(first + first_len - 1, second + first_len - 1);

    let db = Database::open_bytes(bytes).unwrap();
    let err = db.table("t").unwrap().get(b"abcdefgh0").unwrap_err();
    assert_eq!(err.kind(), ErrorKind::CorruptData);
    assert!(err.to_string().contains("sort above"), "{err}");
    assert!(!db.verify(Default::default()).unwrap().is_ok());
}

/// A child reference that names a real page other than the one that belongs there passes
/// every per-page check: the page it names is a valid page of the right kind. Matching
/// each level against the children the level above declares is what finds it.
#[test]
fn verification_reports_a_child_that_points_at_the_wrong_page() {
    let (mut bytes, second_child) = tree_with_several_internal_pages(false);
    let db = Database::open_bytes(bytes.clone()).unwrap();
    let root = db.catalog().tables[0].primary.root.unwrap().get();
    drop(db);

    // Point the root's second child at its first child, so one page of the level below
    // is claimed twice and the next one is never claimed.
    let page = page_offset(&bytes, root);
    let meta = meta_base(&bytes, page);
    let first_end = u16::from_le_bytes(bytes[meta + 2..meta + 4].try_into().unwrap()) as usize;
    let second_end = u16::from_le_bytes(bytes[meta + 4..meta + 6].try_into().unwrap()) as usize;
    let first_child = i64::from_le_bytes(
        bytes[page + first_end - 8..page + first_end]
            .try_into()
            .unwrap(),
    );
    assert_ne!(first_child as u64, second_child);
    bytes[page + second_end - 8..page + second_end].copy_from_slice(&first_child.to_le_bytes());

    let db = Database::open_bytes(bytes).unwrap();
    let report = db.verify(Default::default()).unwrap();
    assert!(
        !report.is_ok(),
        "verification missed a child pointing at the wrong page"
    );
}

/// Builds a table with a unique secondary index spread over several leaves, and returns
/// the file with the ordinal of the index tree's first leaf.
fn table_with_a_multi_leaf_index() -> (Vec<u8>, u64) {
    let mut builder = DatabaseBuilder::new().page_size(256).unwrap();
    let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    builder
        .add_secondary_index(
            table,
            "by_value",
            true,
            // `ascii` has an inexact digest, so the index pages keep their key bytes and
            // the digest is something a check can disagree with.
            Arc::new(AsciiEncoding),
            Box::new(|_key: &[u8], value: &[u8]| Ok(value.to_vec())),
        )
        .unwrap();
    for i in 0..120i64 {
        builder
            .append(
                table,
                &Int64Encoding::encode(i),
                format!("value-{i:04}").as_bytes(),
            )
            .unwrap();
    }
    let bytes = builder.build_to_vec().unwrap();

    let db = Database::open_bytes(bytes.clone()).unwrap();
    let root = db.catalog().tables[0].secondaries[0].root.unwrap().get();
    drop(db);

    // Descend the leftmost path to the first leaf.
    let mut ordinal = root;
    loop {
        let page = page_offset(&bytes, ordinal);
        let kind_word = i32::from_le_bytes(bytes[page + 4..page + 8].try_into().unwrap());
        if kind_word & 0xFF == 0 {
            return (bytes, ordinal);
        }
        ordinal = internal_child(&bytes, page, 0);
    }
}

/// An index tree used to be verified only by scanning it with a cursor and comparing the
/// count, and both stop at the same break, so a leaf chain cut in half looked consistent.
/// Index trees now go through the same structural walk as a primary tree.
#[test]
fn verification_reports_an_index_leaf_chain_that_stops_early() {
    let (mut bytes, first_leaf) = table_with_a_multi_leaf_index();

    let db = Database::open_bytes(bytes.clone()).unwrap();
    assert!(db.verify(Default::default()).unwrap().is_ok());
    drop(db);

    let page = page_offset(&bytes, first_leaf);
    bytes[page + 20..page + 28].copy_from_slice(&(-1i64).to_le_bytes());

    let db = Database::open_bytes(bytes).unwrap();
    let report = db.verify(Default::default()).unwrap();
    assert!(
        !report.is_ok(),
        "verification missed an index leaf chain that stops early"
    );
}

/// Index leaves used to be read through an ordinary cursor, which honours
/// `validate_digests`. An explicit verification pass is the place these are supposed to
/// be found, so it checks them whatever that option says.
#[test]
fn verification_checks_index_digests_even_with_validation_turned_off() {
    let (mut bytes, first_leaf) = table_with_a_multi_leaf_index();
    let page = page_offset(&bytes, first_leaf);
    bytes[page + 28..page + 36].copy_from_slice(&0u64.to_le_bytes());

    let db = OpenOptions::new()
        .validate_digests(false)
        .open_bytes(bytes)
        .unwrap();
    let report = db.verify(Default::default()).unwrap();
    assert!(
        report.problems.iter().any(|p| p.message.contains("digest")),
        "expected the index digest to be reported, got {:?}",
        report.problems
    );
}

/// A leaf holds only the page number of an overflowed value, so a reference that survives
/// the structural checks can still name a page that is not a blob page, or no page at
/// all. Verification used to read the keys and stop there.
#[test]
fn verification_reports_a_value_page_that_is_not_part_of_the_file() {
    let mut builder = DatabaseBuilder::new().page_size(256).unwrap();
    let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    builder
        .append(table, &Int64Encoding::encode(0), &vec![b'v'; 1024])
        .unwrap();
    let mut bytes = builder.build_to_vec().unwrap();

    let db = Database::open_bytes(bytes.clone()).unwrap();
    assert!(db.verify(Default::default()).unwrap().is_ok());
    assert_eq!(
        db.table("t")
            .unwrap()
            .get(&Int64Encoding::encode(0))
            .unwrap()
            .unwrap()
            .len(),
        1024
    );
    let root = db.catalog().tables[0].primary.root.unwrap().get();
    drop(db);

    // The entry payload is the key followed by the blob page ordinal.
    let page = page_offset(&bytes, root);
    let (start, key_len) = leaf_entry_span(&bytes, page, 0);
    bytes[start + key_len..start + key_len + 8].copy_from_slice(&999_999i64.to_le_bytes());

    let db = Database::open_bytes(bytes).unwrap();
    let report = db.verify(Default::default()).unwrap();
    assert!(
        report.problems.iter().any(|p| p.message.contains("999999")),
        "expected the dangling value page to be reported, got {:?}",
        report.problems
    );
    assert_eq!(
        db.table("t")
            .unwrap()
            .get(&Int64Encoding::encode(0))
            .unwrap_err()
            .kind(),
        ErrorKind::CorruptData
    );
}

/// `max_problems` was checked between pages but not between the entries of one page, so a
/// damaged page could add a problem per entry however low the limit was set.
#[test]
fn verification_stops_at_the_problem_limit_inside_a_page() {
    let mut builder = DatabaseBuilder::new().page_size(4096).unwrap();
    let table = builder.create_table("t", Arc::new(AsciiEncoding)).unwrap();
    // One digest for all of them, so the keys can be shuffled without a digest
    // disagreeing with its own key.
    for i in 0..100u8 {
        let key = format!("abcdefgh{i:03}");
        builder.append(table, key.as_bytes(), &[i]).unwrap();
    }
    let mut bytes = builder.build_to_vec().unwrap();
    let db = Database::open_bytes(bytes.clone()).unwrap();
    let root = db.catalog().tables[0].primary.root.unwrap().get();
    drop(db);

    // Reverse the last byte of every key, so nearly every entry sits below the one
    // before it.
    let page = page_offset(&bytes, root);
    for i in 0..50 {
        let (a, len) = leaf_entry_span(&bytes, page, i);
        let (b, _) = leaf_entry_span(&bytes, page, 99 - i);
        for n in 0..3 {
            bytes.swap(a + len - 1 - n, b + len - 1 - n);
        }
    }

    let db = Database::open_bytes(bytes).unwrap();
    for max_problems in [1usize, 3, 10] {
        let report = db
            .verify(drydb::VerifyOptions {
                max_problems,
                ..Default::default()
            })
            .unwrap();
        assert!(
            report.problems.len() <= max_problems,
            "asked for at most {max_problems} problems, got {}",
            report.problems.len()
        );
        assert!(!report.is_ok());
    }
}

/// Reserving the bound copies used to go straight to the budget without reclaiming, so a
/// range that worked on a fresh database failed on the same database after a scan had
/// filled the cache. Every other reservation evicts first; this one now does too.
#[test]
fn a_range_can_be_opened_after_a_scan_has_filled_the_cache() {
    let bytes = build_i64(2_000, 256, 8);
    let db = OpenOptions::new()
        .memory_budget(64 * 1024)
        .cache_capacity(64 * 1024)
        .cache_shards(1)
        .open_bytes(bytes)
        .unwrap();
    let table = db.table("t").unwrap();

    let lower = Int64Encoding::encode(10);
    let upper = Int64Encoding::encode(20);
    let open_range = |table: &drydb::Table| {
        table.range(
            Bound::Included(&lower[..]),
            Bound::Included(&upper[..]),
            Order::Ascending,
        )
    };
    open_range(&table).expect("a range on a fresh database");

    let mut cursor = table.scan(Order::Ascending).unwrap();
    while cursor.advance().unwrap() {}
    drop(cursor);
    assert!(db.memory_report().charged > 32 * 1024, "the cache is full");

    open_range(&table).expect("the same range once the cache is full");
}

/// Every other check can pass while a separator no longer matches the page it points at:
/// the page numbers line up, the sibling links line up, and each page is ordered. A
/// descent then takes the wrong branch and reports a key that is there as absent.
#[test]
fn verification_reports_a_separator_that_does_not_match_its_child() {
    let (mut bytes, second_child) = tree_with_several_internal_pages(false);
    let db = Database::open_bytes(bytes.clone()).unwrap();
    let root = db.catalog().tables[0].primary.root.unwrap().get();
    drop(db);

    // Raise the second separator by one byte. Nothing else changes.
    let page = page_offset(&bytes, root);
    let meta = meta_base(&bytes, page);
    let start = u16::from_le_bytes(bytes[meta + 2..meta + 4].try_into().unwrap()) as usize & 0x7FFF;
    let end = u16::from_le_bytes(bytes[meta + 4..meta + 6].try_into().unwrap()) as usize;
    let key_end = page + end - 8;
    let separator = &mut bytes[page + start..key_end];
    let last = separator.len() - 1;
    separator[last] += 1;
    // The digest has to keep matching the separator, or the page check finds it first.
    let raised = bytes[page + start..key_end].to_vec();
    let mut digest = [0u8; 8];
    for (i, b) in raised.iter().take(8).enumerate() {
        digest[i] = *b;
    }
    let digest_at = page + 28 + 8;
    bytes[digest_at..digest_at + 8].copy_from_slice(&u64::from_be_bytes(digest).to_le_bytes());

    let db = Database::open_bytes(bytes).unwrap();
    let report = db.verify(Default::default()).unwrap();
    assert!(
        report
            .problems
            .iter()
            .any(|p| p.message.contains("separator for page")),
        "expected the separator to be reported, got {:?}",
        report.problems
    );
    let _ = second_child;
}

/// The problem limit is checked once, where problems are recorded, because one page fails
/// several different checks and a caller that only looks between pages overshoots.
#[test]
fn verification_stops_at_the_problem_limit_across_checks() {
    let mut builder = DatabaseBuilder::new().page_size(256).unwrap();
    let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    for i in 0..3i64 {
        builder
            .append(table, &Int64Encoding::encode(i), &i.to_le_bytes())
            .unwrap();
    }
    let mut bytes = builder.build_to_vec().unwrap();
    let db = Database::open_bytes(bytes.clone()).unwrap();
    let root = db.catalog().tables[0].primary.root.unwrap().get();
    drop(db);

    // Two different checks fail on one page: the left sibling link and a digest.
    let page = page_offset(&bytes, root);
    bytes[page + 12..page + 20].copy_from_slice(&(root as i64).to_le_bytes());
    bytes[page + 28 + 8..page + 28 + 16].copy_from_slice(&0u64.to_le_bytes());

    let db = Database::open_bytes(bytes).unwrap();
    let report = db
        .verify(drydb::VerifyOptions {
            max_problems: 1,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(report.problems.len(), 1, "{:?}", report.problems);
    assert!(report.truncated);
}

/// An internal page only points at pages that hold entries, so a child whose entry count
/// was lost drops every row on it out of every scan. `count` said zero while `get` of a
/// later key still worked, and verification reported nothing.
#[test]
fn verification_reports_a_child_page_with_no_entries() {
    let (mut bytes, second_child) = tree_with_several_internal_pages(false);
    let db = Database::open_bytes(bytes.clone()).unwrap();
    let before = db.table("t").unwrap().count().unwrap();
    assert_eq!(before, 400);
    drop(db);

    // Descend from the root's second child to a leaf and empty it.
    let mut ordinal = second_child;
    let page = loop {
        let page = page_offset(&bytes, ordinal);
        let kind_word = i32::from_le_bytes(bytes[page + 4..page + 8].try_into().unwrap());
        if kind_word & 0xFF == 0 {
            break page;
        }
        ordinal = internal_child(&bytes, page, 0);
    };
    assert!(entry_count(&bytes, page) > 0);
    bytes[page + 8..page + 12].copy_from_slice(&0i32.to_le_bytes());

    let db = Database::open_bytes(bytes).unwrap();
    let report = db.verify(Default::default()).unwrap();
    assert!(
        report
            .problems
            .iter()
            .any(|p| p.message.contains("no entries")),
        "expected the empty child to be reported, got {:?}",
        report.problems
    );
}

/// The same check where the separator is a digest, because the page it is on omits its
/// key bytes. (The mirror case, a separator with key bytes over a child that omits its
/// own, is handled in `check_separator` but is not reachable from a file either builder
/// writes: both choose one layout for a whole tree.)
#[test]
fn verification_reports_a_digest_separator_that_does_not_match_its_child() {
    // `i64` has an exact digest, so the leaves omit their keys; the page size is above
    // the compact limit, so the internal pages use classic metadata and keep theirs.
    let mut builder = DatabaseBuilder::new().page_size(256).unwrap();
    let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    for i in 0..40i64 {
        builder
            .append(table, &Int64Encoding::encode(i), &i.to_le_bytes())
            .unwrap();
    }
    let mut bytes = builder.build_to_vec().unwrap();

    let db = Database::open_bytes(bytes.clone()).unwrap();
    assert!(db.verify(Default::default()).unwrap().is_ok());
    let root = db.catalog().tables[0].primary.root.unwrap().get();
    let page = page_offset(&bytes, root);
    let kind_word = i32::from_le_bytes(bytes[page + 4..page + 8].try_into().unwrap());
    assert_eq!(kind_word & 0xFF, 1, "the root has to be an internal page");
    let omitted = kind_word & (1 << 11) != 0;
    drop(db);

    // Raise the second separator's digest by one. On a page that omits keys the digest
    // is the separator, so this is the whole separator.
    let at = page + 28 + 8;
    let digest = u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
    bytes[at..at + 8].copy_from_slice(&(digest + 1).to_le_bytes());
    if !omitted {
        // Keep the key bytes in step with the digest, or the page check finds it first.
        let meta = meta_base(&bytes, page);
        let start =
            u16::from_le_bytes(bytes[meta + 2..meta + 4].try_into().unwrap()) as usize & 0x7FFF;
        let key = i64::from_le_bytes(bytes[page + start..page + start + 8].try_into().unwrap());
        bytes[page + start..page + start + 8].copy_from_slice(&(key + 1).to_le_bytes());
    }

    let db = Database::open_bytes(bytes).unwrap();
    let report = db.verify(Default::default()).unwrap();
    assert!(
        report
            .problems
            .iter()
            .any(|p| p.message.contains("separator for page")),
        "expected the separator to be reported, got {:?}",
        report.problems
    );
}

/// A key encoding that can rebuild its keys from a digest without having a fixed width.
///
/// Keys are empty or one byte. A fixed width is what lets a buffer be reserved before the
/// rebuild writes into it, but it is not what makes the rebuild possible, and requiring
/// one turned every scan of such a table into an error.
#[derive(Debug)]
struct ShortKey;

impl drydb::KeyEncoding for ShortKey {
    fn id(&self) -> &str {
        "short-key"
    }

    fn compare(&self, a: &[u8], b: &[u8]) -> Result<std::cmp::Ordering, drydb::Error> {
        Ok(a.len().cmp(&b.len()).then_with(|| a.cmp(b)))
    }

    fn digest(&self, key: &[u8]) -> Result<u64, drydb::Error> {
        Ok(match key {
            [] => 0,
            [b] => *b as u64 + 1,
            _ => {
                return Err(drydb::Error::new(
                    ErrorKind::InvalidArgument,
                    "short keys are at most one byte",
                ))
            }
        })
    }

    fn is_digest_exact(&self) -> bool {
        true
    }

    fn decode_key_from_digest(&self, digest: u64, out: &mut Vec<u8>) -> Result<(), drydb::Error> {
        out.clear();
        match digest {
            0 => {}
            d if d <= 256 => out.push((d - 1) as u8),
            _ => {
                return Err(drydb::Error::new(
                    ErrorKind::CorruptData,
                    "digest is not a short key",
                ))
            }
        }
        Ok(())
    }

    fn max_rebuilt_key_len(&self) -> Option<usize> {
        Some(1)
    }

    fn validate_key(&self, key: &[u8]) -> Result<(), drydb::Error> {
        if key.len() > 1 {
            return Err(drydb::Error::new(
                ErrorKind::InvalidArgument,
                "short keys are at most one byte",
            ));
        }
        Ok(())
    }
}

#[test]
fn keys_can_be_rebuilt_by_an_encoding_with_no_fixed_width() {
    let mut builder = DatabaseBuilder::new().page_size(256).unwrap();
    let table = builder.create_table("t", Arc::new(ShortKey)).unwrap();
    builder.append(table, b"", b"empty").unwrap();
    for i in 0..8u8 {
        builder.append(table, &[i], &[i]).unwrap();
    }
    let bytes = builder.build_to_vec().unwrap();

    let db = OpenOptions::new()
        .register_encoding(Arc::new(ShortKey))
        .unwrap()
        .open_bytes(bytes)
        .unwrap();
    let table = db.table("t").unwrap();
    assert!(db.verify(Default::default()).unwrap().is_ok());
    assert_eq!(table.get(b"").unwrap().unwrap().as_ref(), b"empty");
    assert_eq!(table.get(&[3u8]).unwrap().unwrap().as_ref(), &[3u8]);

    let mut cursor = table.scan(Order::Ascending).unwrap();
    let mut keys = Vec::new();
    while cursor.advance().unwrap() {
        keys.push(cursor.current().unwrap().key().to_vec());
    }
    let expected: Vec<Vec<u8>> = std::iter::once(Vec::new())
        .chain((0..8u8).map(|i| vec![i]))
        .collect();
    assert_eq!(keys, expected);
}

/// A digest covers at most the first eight bytes of a key, so a key that lost the rest of
/// itself still digests to what the page says. A 16-byte ULID key cut to 8 made `get` of
/// the real key return `Ok(None)` and a scan hand back a key that is not one. Stored keys
/// are now checked against the encoding the same way a caller's key is.
#[test]
fn a_stored_key_of_the_wrong_width_is_rejected() {
    let mut builder = DatabaseBuilder::new().page_size(256).unwrap();
    let table = builder.create_table("t", Arc::new(UlidEncoding)).unwrap();
    let key = *b"abcdefgh01234567";
    builder.append(table, &key, b"v").unwrap();
    let mut bytes = builder.build_to_vec().unwrap();

    let db = Database::open_bytes(bytes.clone()).unwrap();
    assert!(db.table("t").unwrap().get(&key).unwrap().is_some());
    let root = db.catalog().tables[0].primary.root.unwrap().get();
    drop(db);

    // Halve the stored key length. The first eight bytes, and so the digest, are intact.
    let page = page_offset(&bytes, root);
    let meta = meta_base(&bytes, page);
    let entries = entry_count(&bytes, page);
    let key_len_at = meta + (entries + 1) * 2;
    assert_eq!(
        u16::from_le_bytes(bytes[key_len_at..key_len_at + 2].try_into().unwrap()),
        16
    );
    bytes[key_len_at..key_len_at + 2].copy_from_slice(&8u16.to_le_bytes());

    let db = Database::open_bytes(bytes).unwrap();
    let err = db.table("t").unwrap().get(&key).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::CorruptData);
    assert!(!db.verify(Default::default()).unwrap().is_ok());
}

/// A secondary index entry that points at the page header rather than at a record used to
/// hand the header bytes back as the value. The read path refuses a reference that starts
/// inside the prefix, and verification goes further and checks it against the records on
/// the page it names.
#[test]
fn an_index_reference_that_does_not_point_at_a_record_is_rejected() {
    let mut builder = DatabaseBuilder::new().page_size(256).unwrap();
    let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    builder
        .add_secondary_index(
            table,
            "by_key",
            true,
            Arc::new(Int64Encoding),
            Box::new(|key: &[u8], _value: &[u8]| Ok(key.to_vec())),
        )
        .unwrap();
    builder
        .append(table, &Int64Encoding::encode(0), b"record-v")
        .unwrap();
    let bytes = builder.build_to_vec().unwrap();

    let db = Database::open_bytes(bytes.clone()).unwrap();
    let index_root = db.catalog().tables[0].secondaries[0].root.unwrap().get();
    let mut cursor = db
        .table("t")
        .unwrap()
        .index("by_key")
        .unwrap()
        .scan(Order::Ascending)
        .unwrap();
    assert!(cursor.advance().unwrap());
    assert_eq!(cursor.value().unwrap().as_ref(), b"record-v");
    drop(cursor);
    drop(db);

    // The index entry's value is a 16-byte PageRef: ordinal, start, length.
    let page = page_offset(&bytes, index_root);
    let (start, key_len) = leaf_entry_span(&bytes, page, 0);
    let reference_at = start + key_len;
    for (case, patched_start) in [("header", 0i32), ("between records", 30i32)] {
        let mut bytes = bytes.clone();
        bytes[reference_at + 8..reference_at + 12].copy_from_slice(&patched_start.to_le_bytes());

        let db = Database::open_bytes(bytes).unwrap();
        let mut cursor = db
            .table("t")
            .unwrap()
            .index("by_key")
            .unwrap()
            .scan(Order::Ascending)
            .unwrap();
        let outcome = cursor.advance();
        let report = db.verify(Default::default()).unwrap();
        assert!(
            outcome.is_err() || !report.is_ok(),
            "[{case}] a reference that is not a record's value was accepted, and \
             verification reported {:?}",
            report.problems
        );
    }
}

/// An encoding that rebuilds keys of a length it does not declare cannot have its buffer
/// reserved before the rebuild writes into it, which is the one thing the memory budget
/// cannot cover. Such a page is refused rather than read.
#[derive(Debug)]
struct UnboundedKey;

impl drydb::KeyEncoding for UnboundedKey {
    fn id(&self) -> &str {
        "unbounded-key"
    }

    fn compare(&self, a: &[u8], b: &[u8]) -> Result<std::cmp::Ordering, drydb::Error> {
        Ok(a.len().cmp(&b.len()))
    }

    fn digest(&self, key: &[u8]) -> Result<u64, drydb::Error> {
        Ok(key.len() as u64)
    }

    fn is_digest_exact(&self) -> bool {
        true
    }

    fn decode_key_from_digest(&self, digest: u64, out: &mut Vec<u8>) -> Result<(), drydb::Error> {
        out.clear();
        out.resize(digest as usize, b'x');
        Ok(())
    }
}

#[test]
fn a_rebuild_with_no_declared_size_is_refused_rather_than_allocated() {
    let mut builder = DatabaseBuilder::new().page_size(256).unwrap();
    let table = builder.create_table("t", Arc::new(UnboundedKey)).unwrap();
    for len in [1usize, 2, 3] {
        builder
            .append(table, &vec![b'x'; len], &[len as u8])
            .unwrap();
    }
    let bytes = builder.build_to_vec().unwrap();

    let db = OpenOptions::new()
        .register_encoding(Arc::new(UnboundedKey))
        .unwrap()
        .memory_budget(64 * 1024)
        .open_bytes(bytes)
        .unwrap();
    let mut cursor = db.table("t").unwrap().scan(Order::Ascending).unwrap();
    let err = cursor.advance().unwrap_err();
    assert_eq!(err.kind(), ErrorKind::CorruptData);
    assert!(err.to_string().contains("max_rebuilt_key_len"), "{err}");
}

/// An index entry whose own reference did not fit on the page keeps it on a blob page.
/// The reference check only looked at inline values, so those went unchecked and an index
/// lookup handed back a value that was a byte short while verification passed.
#[test]
fn verification_checks_an_index_reference_kept_on_a_blob_page() {
    let mut builder = DatabaseBuilder::new().page_size(256).unwrap();
    let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    builder
        .add_secondary_index(
            table,
            "by_key",
            true,
            Arc::new(Int64Encoding),
            Box::new(|key: &[u8], _value: &[u8]| Ok(key.to_vec())),
        )
        .unwrap();
    for i in 0..40i64 {
        builder
            .append(table, &Int64Encoding::encode(i), &i.to_le_bytes())
            .unwrap();
    }
    let mut bytes = builder.build_to_vec().unwrap();

    let db = Database::open_bytes(bytes.clone()).unwrap();
    assert!(db.verify(Default::default()).unwrap().is_ok());
    let pages = db.catalog().page_count;
    let index_root = db.catalog().tables[0].secondaries[0].root.unwrap().get();
    drop(db);

    // Find an index leaf entry whose reference went to a blob page. The index pages omit
    // their keys, so an entry's payload is the blob page ordinal on its own.
    let mut patched = None;
    'pages: for ordinal in 0..pages {
        let page = page_offset(&bytes, ordinal);
        let kind_word = i32::from_le_bytes(bytes[page + 4..page + 8].try_into().unwrap());
        if kind_word & 0xFF != 0 || kind_word & (1 << 11) == 0 {
            continue;
        }
        let meta = meta_base(&bytes, page);
        for i in 0..entry_count(&bytes, page) {
            let slot =
                u16::from_le_bytes(bytes[meta + i * 2..meta + i * 2 + 2].try_into().unwrap());
            if slot & 0x8000 == 0 {
                continue;
            }
            let at = page + (slot & 0x7FFF) as usize;
            let blob = i64::from_le_bytes(bytes[at..at + 8].try_into().unwrap()) as u64;
            // The blob payload is the sixteen byte PageRef: ordinal, start, length.
            let length_at = page_offset(&bytes, blob) + 28 + 12;
            let length = i32::from_le_bytes(bytes[length_at..length_at + 4].try_into().unwrap());
            patched = Some((length_at, length));
            break 'pages;
        }
    }
    let (length_at, length) = patched.expect("an index reference on a blob page");
    assert_eq!(length, 8, "the record values are eight bytes");
    let _ = index_root;
    bytes[length_at..length_at + 4].copy_from_slice(&(length - 1).to_le_bytes());

    let db = Database::open_bytes(bytes).unwrap();
    let report = db.verify(Default::default()).unwrap();
    assert!(
        !report.is_ok(),
        "verification missed a shortened reference on a blob page"
    );
}

/// A non-unique index appends a record id to its source encoding's keys, and a page of
/// such an index can omit them: that is what the C# builder writes when the source digest
/// is exact. The bound on a rebuilt key then has to come from the source encoding plus
/// those four bytes. Falling back to the fixed width made every scan of such an index
/// fail for a source encoding whose keys vary in length.
#[test]
fn a_non_unique_index_inherits_the_rebuild_bound_of_its_source() {
    let source = Arc::new(ShortKey);
    assert_eq!(drydb::KeyEncoding::fixed_key_len(source.as_ref()), None);

    let mut builder = DatabaseBuilder::new().page_size(256).unwrap();
    let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    builder
        .add_secondary_index(
            table,
            "by_value",
            true,
            Arc::clone(&source) as Arc<dyn drydb::KeyEncoding>,
            Box::new(|_key: &[u8], value: &[u8]| Ok(value.to_vec())),
        )
        .unwrap();
    for i in 0..6u8 {
        builder
            .append(table, &Int64Encoding::encode(i as i64), &[i])
            .unwrap();
    }
    let mut bytes = builder.build_to_vec().unwrap();

    let open = |bytes: Vec<u8>| {
        OpenOptions::new()
            .register_encoding(Arc::new(ShortKey))
            .unwrap()
            .open_bytes(bytes)
    };

    let db = open(bytes.clone()).unwrap();
    let descriptor = &db.catalog().tables[0].secondaries[0];
    assert!(descriptor.is_unique);
    // The descriptor lays out `is_unique`, `value_kind`, then the root ordinal.
    let unique_at = descriptor.root_field_offset as usize - 2;
    drop(db);

    // Make it a non-unique index, which is the layout the C# builder produces: composite
    // keys with the key bytes left off the page.
    bytes[unique_at] = 0;
    let db = open(bytes).unwrap();
    let index = db.table("t").unwrap().index("by_value").unwrap();
    let mut cursor = index.scan(Order::Ascending).unwrap();
    let mut seen = 0;
    while cursor.advance().unwrap() {
        seen += 1;
    }
    assert_eq!(seen, 6);
    assert!(db.verify(Default::default()).unwrap().is_ok());
}

/// A search starts at the root and can only go down and to the right of where it lands,
/// so a root with a page beside it is a tree whose other pages no search reaches. Every
/// other check passes on such a file: the levels below are intact, they are just not all
/// under this root.
#[test]
fn verification_reports_a_root_with_a_sibling() {
    let mut builder = DatabaseBuilder::new().page_size(128).unwrap();
    let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    for i in 0..1000i64 {
        builder
            .append(table, &Int64Encoding::encode(i), &i.to_le_bytes())
            .unwrap();
    }
    let mut bytes = builder.build_to_vec().unwrap();

    let db = Database::open_bytes(bytes.clone()).unwrap();
    assert!(db.verify(Default::default()).unwrap().is_ok());
    let descriptor = &db.catalog().tables[0].primary;
    let root = descriptor.root.unwrap().get();
    let root_field = descriptor.root_field_offset as usize;
    drop(db);

    // Point the descriptor at the root's first child, which is a page with siblings.
    let child = internal_child(&bytes, page_offset(&bytes, root), 0);
    assert_ne!(child, root);
    bytes[root_field..root_field + 8].copy_from_slice(&(child as i64).to_le_bytes());

    let db = Database::open_bytes(bytes).unwrap();
    assert!(db
        .table("t")
        .unwrap()
        .get(&Int64Encoding::encode(999))
        .unwrap()
        .is_none());
    let report = db.verify(Default::default()).unwrap();
    assert!(
        report
            .problems
            .iter()
            .any(|p| p.message.contains("root has a sibling")),
        "expected the truncated tree to be reported, got {:?}",
        report.problems
    );
}

/// The page check covers one page at a time, so a run of pages whose digests each ascend
/// can still step backwards at a boundary. On a page that omits its keys the digest is
/// the only thing there is to compare, so nothing else catches it.
#[test]
fn verification_reports_digests_that_fall_between_leaves() {
    let mut builder = DatabaseBuilder::new().page_size(256).unwrap();
    let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    builder
        .add_secondary_index(
            table,
            "by_key",
            true,
            Arc::new(Int64Encoding),
            Box::new(|key: &[u8], _value: &[u8]| Ok(key.to_vec())),
        )
        .unwrap();
    for i in 0..40i64 {
        builder
            .append(table, &Int64Encoding::encode(i), &i.to_le_bytes())
            .unwrap();
    }
    let mut bytes = builder.build_to_vec().unwrap();

    let db = Database::open_bytes(bytes.clone()).unwrap();
    assert!(db.verify(Default::default()).unwrap().is_ok());
    let descriptor = &db.catalog().tables[0].secondaries[0];
    let index_root = descriptor.root.unwrap().get();
    let unique_at = descriptor.root_field_offset as usize - 2;
    drop(db);

    // The C#-compatible shape: a non-unique index whose pages omit their keys.
    bytes[unique_at] = 0;

    // Raise the last digest of the first index leaf above the ones that follow it.
    let mut ordinal = index_root;
    let leaf = loop {
        let page = page_offset(&bytes, ordinal);
        let kind_word = i32::from_le_bytes(bytes[page + 4..page + 8].try_into().unwrap());
        if kind_word & 0xFF == 0 {
            break page;
        }
        ordinal = internal_child(&bytes, page, 0);
    };
    let last = entry_count(&bytes, leaf) - 1;
    let at = leaf + 28 + last * 8;
    let digest = u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
    bytes[at..at + 8].copy_from_slice(&(digest + 1_000).to_le_bytes());

    let db = Database::open_bytes(bytes).unwrap();
    let report = db.verify(Default::default()).unwrap();
    assert!(
        !report.is_ok(),
        "verification missed a digest that falls between leaves"
    );
}

/// Record ids count up from zero within an index key, and a lookup is the composite range
/// `(k, 0)..=(k, i32::MAX)`. A negative one sorts below that whole range, so the row it
/// belongs to was on the page and in the count but outside every lookup.
#[test]
fn a_negative_record_id_in_an_index_key_is_rejected() {
    let mut builder = DatabaseBuilder::new().page_size(256).unwrap();
    let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    builder
        .add_secondary_index(
            table,
            "by_value",
            false,
            Arc::new(AsciiEncoding),
            Box::new(|_key: &[u8], _value: &[u8]| Ok(b"a".to_vec())),
        )
        .unwrap();
    builder
        .append(table, &Int64Encoding::encode(0), b"value")
        .unwrap();
    let mut bytes = builder.build_to_vec().unwrap();

    let db = Database::open_bytes(bytes.clone()).unwrap();
    let index_root = db.catalog().tables[0].secondaries[0].root.unwrap().get();
    let mut cursor = db
        .table("t")
        .unwrap()
        .index("by_value")
        .unwrap()
        .lookup(b"a")
        .unwrap();
    assert!(cursor.advance().unwrap());
    drop(cursor);
    drop(db);

    // The composite key is the source key followed by the record id.
    let page = page_offset(&bytes, index_root);
    let (start, key_len) = leaf_entry_span(&bytes, page, 0);
    let rid_at = start + key_len - 4;
    assert_eq!(
        i32::from_le_bytes(bytes[rid_at..rid_at + 4].try_into().unwrap()),
        0
    );
    bytes[rid_at..rid_at + 4].copy_from_slice(&(-1i32).to_le_bytes());

    let db = Database::open_bytes(bytes).unwrap();
    let index = db.table("t").unwrap().index("by_value").unwrap();
    let outcome = index.lookup(b"a").and_then(|mut c| c.advance());
    let report = db.verify(Default::default()).unwrap();
    assert!(
        outcome.is_err() || !report.is_ok(),
        "a row filed under a negative record id was accepted, and verification reported \
         {:?}",
        report.problems
    );
}

/// A page with no key bytes is read entirely through its digests, which only means
/// anything for an encoding that can turn a digest back into a key. Under any other
/// encoding a search matched a key that is not on the page and handed back the row it
/// landed on, instead of reporting the page as the nonsense it is.
#[test]
fn a_page_that_omits_keys_under_an_encoding_that_cannot_rebuild_them_is_rejected() {
    let mut builder = DatabaseBuilder::new().page_size(256).unwrap();
    let table = builder.create_table("t", Arc::new(AsciiEncoding)).unwrap();
    builder.append(table, b"abcdefgh", b"value").unwrap();
    let mut bytes = builder.build_to_vec().unwrap();

    let db = Database::open_bytes(bytes.clone()).unwrap();
    assert!(db.table("t").unwrap().get(b"abcdefgh").unwrap().is_some());
    let root = db.catalog().tables[0].primary.root.unwrap().get();
    drop(db);

    // Claim the page omits its keys. `ascii` cannot rebuild one from a digest, so every
    // comparison on this page is a digest comparison and nothing checks the keys.
    let page = page_offset(&bytes, root);
    let kind_word = i32::from_le_bytes(bytes[page + 4..page + 8].try_into().unwrap());
    bytes[page + 4..page + 8].copy_from_slice(&(kind_word | (1 << 11)).to_le_bytes());

    let db = Database::open_bytes(bytes).unwrap();
    let table = db.table("t").unwrap();
    let err = table.get(b"abcdefgh").unwrap_err();
    assert_eq!(err.kind(), ErrorKind::CorruptData);
    assert!(err.to_string().contains("max_rebuilt_key_len"), "{err}");
    assert!(table.get(b"zzzzzzzz").is_err());
    assert!(!db.verify(Default::default()).unwrap().is_ok());
}

/// The report a verification pass hands back is memory the pass spends on the caller's
/// behalf, and `max_problems` only bounds how many problems there are. Messages that
/// carry formatted keys and a table name made a hundred of them come to 121,220 bytes
/// inside a 64 KiB budget, so each message is bounded too, in what it says and in what
/// it holds.
#[test]
fn verification_bounds_the_size_of_its_report() {
    let mut builder = DatabaseBuilder::new().page_size(16384).unwrap();
    // A long table name, which every message of this tree carries.
    let table = builder
        .create_table("t".repeat(600), Arc::new(AsciiEncoding))
        .unwrap();
    for i in 0..20u16 {
        // Keys sharing their first eight bytes, so they share a digest and can be put
        // out of order without a digest disagreeing with its own key.
        let key = format!("kkkkkkkk{}{i:04}", "k".repeat(100));
        builder.append(table, key.as_bytes(), &[0]).unwrap();
    }
    let mut bytes = builder.build_to_vec().unwrap();
    let db = Database::open_bytes(bytes.clone()).unwrap();
    assert!(db.verify(Default::default()).unwrap().is_ok());
    let root = db.catalog().tables[0].primary.root.unwrap().get();
    drop(db);

    let page = page_offset(&bytes, root);
    let entries = entry_count(&bytes, page);
    assert!(entries >= 2, "the keys have to share one leaf");
    let (a, len) = leaf_entry_span(&bytes, page, 0);
    let (b, _) = leaf_entry_span(&bytes, page, 1);
    for n in 0..4 {
        bytes.swap(a + len - 1 - n, b + len - 1 - n);
    }

    let db = Database::open_bytes(bytes).unwrap();
    let report = db.verify(Default::default()).unwrap();
    assert!(!report.is_ok());
    let longest = report
        .problems
        .iter()
        .map(|p| p.message.len())
        .max()
        .unwrap();
    assert!(
        longest <= 600,
        "the longest problem message is {longest} bytes"
    );
    let widest = report
        .problems
        .iter()
        .map(|p| p.message.capacity())
        .max()
        .unwrap();
    assert!(
        widest <= 600,
        "a message holds {widest} bytes of capacity; the bound has to be on what is \
         allocated, not only on what is shown"
    );
    assert!(
        report
            .problems
            .iter()
            .all(|p| !p.message.contains(&"t".repeat(100))),
        "a 600 character table name should not be carried into a message in full"
    );
}

/// Streaming a blob reserves its transfer buffer, and that reservation used to go
/// straight to the budget without reclaiming: the same copy worked on a fresh database
/// and failed once a scan had filled the cache.
#[test]
fn a_blob_copy_works_after_a_scan_has_filled_the_cache() {
    let mut builder = DatabaseBuilder::new().page_size(256).unwrap();
    let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    for i in 0..200i64 {
        let value = if i == 0 {
            vec![b'v'; 1024]
        } else {
            i.to_le_bytes().to_vec()
        };
        builder
            .append(table, &Int64Encoding::encode(i), &value)
            .unwrap();
    }
    let bytes = builder.build_to_vec().unwrap();

    let db = OpenOptions::new()
        .memory_budget(16 * 1024)
        .cache_capacity(16 * 1024)
        .cache_shards(1)
        .open_bytes(bytes)
        .unwrap();
    let table = db.table("t").unwrap();
    let key = Int64Encoding::encode(0);

    let copy = || {
        table
            .blob_reader(&key)
            .unwrap()
            .expect("the value is on a blob page")
            .copy_to(&mut std::io::sink(), 4096)
    };
    assert_eq!(copy().expect("a copy on a fresh database"), 1024);

    let mut cursor = table.scan(Order::Ascending).unwrap();
    while cursor.advance().unwrap() {}
    drop(cursor);
    assert!(db.memory_report().charged > 8 * 1024, "the cache is full");

    assert_eq!(copy().expect("the same copy once the cache is full"), 1024);
}

/// A fixed key width is not enough to read a page that omits its keys: `uuidv7` and
/// `ulid` keys are sixteen bytes and their digest covers the first eight, so no digest
/// determines a key. Such a page used to answer a point lookup for a key that is not on
/// it with whatever row the digest landed on, while a scan of the same page failed.
#[test]
fn an_omitted_key_page_under_a_fixed_width_encoding_that_cannot_rebuild_is_rejected() {
    for encoding in ["uuidv7", "ulid"] {
        let key = [0u8; 16];
        let mut other = key;
        other[15] = 1;
        let mut builder = DatabaseBuilder::new().page_size(256).unwrap();
        let table = builder
            .create_table(
                "t",
                if encoding == "ulid" {
                    Arc::new(UlidEncoding) as Arc<dyn drydb::KeyEncoding>
                } else {
                    Arc::new(drydb::Uuidv7Encoding)
                },
            )
            .unwrap();
        builder.append(table, &key, &[42]).unwrap();
        let mut bytes = builder.build_to_vec().unwrap();

        let db = Database::open_bytes(bytes.clone()).unwrap();
        assert!(db.table("t").unwrap().get(&key).unwrap().is_some());
        let root = db.catalog().tables[0].primary.root.unwrap().get();
        drop(db);

        // Claim the page omits its keys.
        let page = page_offset(&bytes, root);
        let kind_word = i32::from_le_bytes(bytes[page + 4..page + 8].try_into().unwrap());
        bytes[page + 4..page + 8].copy_from_slice(&(kind_word | (1 << 11)).to_le_bytes());

        let db = Database::open_bytes(bytes).unwrap();
        let table = db.table("t").unwrap();
        for probe in [key, other] {
            let err = table.get(&probe).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::CorruptData, "[{encoding}]");
            assert!(err.to_string().contains("max_rebuilt_key_len"), "{err}");
        }
        assert!(!db.verify(Default::default()).unwrap().is_ok());
    }
}

/// On a page that omits its key bytes the digest is the key, so two entries with the same
/// digest are two rows under one key. A search stops at the first of them and the rows
/// behind it are on the page and in the count but out of reach.
#[test]
fn duplicate_digests_on_an_omitted_key_page_are_rejected() {
    let mut builder = DatabaseBuilder::new().page_size(4096).unwrap();
    let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    for i in 0..3i64 {
        builder
            .append(table, &Int64Encoding::encode(i), &[i as u8])
            .unwrap();
    }
    let mut bytes = builder.build_to_vec().unwrap();

    let db = Database::open_bytes(bytes.clone()).unwrap();
    let root = db.catalog().tables[0].primary.root.unwrap().get();
    let page = page_offset(&bytes, root);
    let kind_word = i32::from_le_bytes(bytes[page + 4..page + 8].try_into().unwrap());
    assert_ne!(kind_word & (1 << 11), 0, "`i64` pages omit their keys");
    drop(db);

    // Repeat the first digest, so the second row can no longer be found.
    let first = u64::from_le_bytes(bytes[page + 28..page + 36].try_into().unwrap());
    bytes[page + 36..page + 44].copy_from_slice(&first.to_le_bytes());

    let db = Database::open_bytes(bytes).unwrap();
    let table = db.table("t").unwrap();
    let err = table.get(&Int64Encoding::encode(0)).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::CorruptData);
    assert!(err.to_string().contains("repeats"), "{err}");
    assert!(table.count().is_err());
    assert!(!db.verify(Default::default()).unwrap().is_ok());
}

/// Reclaiming used to stop after a fixed number of rounds. The page directory gives back
/// one chunk at a time, so a read that needed many of them was refused with the memory
/// still there, and the same call succeeded on the nineteenth try.
#[test]
fn a_large_value_is_read_on_the_first_try_however_much_has_to_be_reclaimed() {
    let mut builder = DatabaseBuilder::new().page_size(128).unwrap();
    let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    for i in 0..3000i64 {
        builder
            .append(table, &Int64Encoding::encode(i), &[b'v'; 60])
            .unwrap();
    }
    builder
        .append(table, &Int64Encoding::encode(3000), &vec![b'w'; 58_000])
        .unwrap();
    let bytes = builder.build_to_vec().unwrap();

    let db = OpenOptions::new()
        // Room for the large value and little else, so reading it has to reclaim the
        // directory chunks the scan left resident, one at a time.
        .memory_budget(131_072)
        .cache_capacity(0)
        .cache_shards(1)
        .directory_chunk_entries(1)
        .directory_chunks(100_000)
        .open_bytes(bytes)
        .unwrap();
    let table = db.table("t").unwrap();

    // Fill the directory with chunks by reading every small value.
    let mut cursor = table.scan(Order::Ascending).unwrap();
    while cursor.advance().unwrap() {}
    drop(cursor);

    let value = table
        .get(&Int64Encoding::encode(3000))
        .expect("the large value has to be readable without a retry")
        .expect("the row is there");
    assert_eq!(value.len(), 58_000);
}

/// A cursor that could not read the entry it had just moved to used to stay moved, so a
/// caller who freed memory and tried again got the row after the one that failed and
/// never saw the one in between. A failed step is now undone.
#[test]
fn a_cursor_retried_after_a_budget_error_returns_the_row_it_could_not_read() {
    let mut builder = DatabaseBuilder::new().page_size(256).unwrap();
    let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    builder
        .append(table, &Int64Encoding::encode(0), &vec![b'a'; 9_000])
        .unwrap();
    builder
        .append(table, &Int64Encoding::encode(1), b"one")
        .unwrap();
    builder
        .append(table, &Int64Encoding::encode(2), &vec![b'c'; 6_000])
        .unwrap();
    let bytes = builder.build_to_vec().unwrap();

    let db = OpenOptions::new()
        .memory_budget(16 * 1024)
        .open_bytes(bytes)
        .unwrap();
    let table = db.table("t").unwrap();

    // Hold the last row, leaving too little room for the first.
    let held = table.get(&Int64Encoding::encode(2)).unwrap().unwrap();
    let mut cursor = table.scan(Order::Ascending).unwrap();
    let refused = cursor.advance().unwrap_err();
    assert_eq!(refused.kind(), ErrorKind::BudgetExceeded);
    assert!(refused.is_retryable());

    drop(held);
    assert!(cursor.advance().unwrap(), "the retry has to produce a row");
    assert_eq!(
        cursor.current().unwrap().key(),
        &Int64Encoding::encode(0),
        "the retry has to return the row the first call could not read"
    );

    // And the rest of the scan follows it.
    let mut keys = vec![0i64];
    while cursor.advance().unwrap() {
        keys.push(i64::from_le_bytes(
            cursor.current().unwrap().key().try_into().unwrap(),
        ));
    }
    assert_eq!(keys, vec![0, 1, 2]);
}

/// A pass that could not record what it found did not find that the file is sound; it
/// found that it stopped. Reporting no problems with `truncated` set read as a clean
/// file.
#[test]
fn a_truncated_verification_is_not_a_pass() {
    let mut builder = DatabaseBuilder::new().page_size(128).unwrap();
    let table = builder.create_table("t", Arc::new(AsciiEncoding)).unwrap();
    builder.append(table, b"a", b"v").unwrap();
    let mut bytes = builder.build_to_vec().unwrap();

    let db = Database::open_bytes(bytes.clone()).unwrap();
    let root = db.catalog().tables[0].primary.root.unwrap().get();
    drop(db);

    // An unknown page flag, which every reader refuses.
    let page = page_offset(&bytes, root);
    let kind_word = i32::from_le_bytes(bytes[page + 4..page + 8].try_into().unwrap());
    bytes[page + 4..page + 8].copy_from_slice(&(kind_word | 0x1000_0000).to_le_bytes());

    let db = OpenOptions::new()
        .memory_budget(16 * 1024)
        .open_bytes(bytes)
        .unwrap();
    assert!(db.table("t").unwrap().get(b"a").is_err());

    // Hold almost the whole budget, so there is no room to record what the pass finds.
    let hog = vec![b'k'; 9_208];
    let _cursor = db.table("t").unwrap().range(
        Bound::Included(&hog[..]),
        Bound::Unbounded,
        Order::Ascending,
    );
    let report = db.verify(Default::default()).unwrap();
    assert!(
        !report.is_ok(),
        "a pass that stopped early must not read as a clean file: {report:?}"
    );
}

/// A secondary cursor steps its inner cursor and then resolves the reference the entry
/// points at. The inner cursor undoes its own failures, but that resolution happens after
/// it has already moved, so a caller who freed memory and tried again got the row after
/// the one that failed.
#[test]
fn an_index_cursor_retried_after_a_budget_error_returns_the_row_it_could_not_read() {
    let mut builder = DatabaseBuilder::new().page_size(256).unwrap();
    let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    builder
        .add_secondary_index(
            table,
            "by_key",
            true,
            Arc::new(Int64Encoding),
            Box::new(|key: &[u8], _value: &[u8]| Ok(key.to_vec())),
        )
        .unwrap();
    for (i, len) in [(0i64, 19_000usize), (1, 3), (2, 10_000)] {
        builder
            .append(table, &Int64Encoding::encode(i), &vec![b'v'; len])
            .unwrap();
    }
    let bytes = builder.build_to_vec().unwrap();

    let db = OpenOptions::new()
        .memory_budget(32 * 1024)
        .open_bytes(bytes)
        .unwrap();
    let table = db.table("t").unwrap();
    let index = table.index("by_key").unwrap();

    let held = table.get(&Int64Encoding::encode(2)).unwrap().unwrap();
    let mut cursor = index.scan(Order::Ascending).unwrap();
    let refused = cursor.advance().unwrap_err();
    assert_eq!(refused.kind(), ErrorKind::BudgetExceeded);

    drop(held);
    let mut keys = Vec::new();
    while cursor.advance().unwrap() {
        keys.push(i64::from_le_bytes(cursor.key().try_into().unwrap()));
    }
    assert_eq!(
        keys,
        vec![0, 1, 2],
        "the retry has to return the row the first call could not read"
    );
}

/// A tree too deep for a search to descend used to pass verification: the pass counted
/// only the internal levels while a search counts every page it reads on the way down,
/// so the two disagreed about the same file by exactly one level.
#[test]
fn verification_and_search_agree_about_a_tree_that_is_too_deep() {
    let mut builder = DatabaseBuilder::new().page_size(128).unwrap();
    let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    builder
        .append(table, &Int64Encoding::encode(0), &[7])
        .unwrap();
    let bytes = builder.build_to_vec().unwrap();

    let db = Database::open_bytes(bytes.clone()).unwrap();
    let descriptor = &db.catalog().tables[0].primary;
    let leaf = descriptor.root.unwrap().get();
    let root_field = descriptor.root_field_offset as usize;
    let page_count = db.catalog().page_count;
    drop(db);

    // Read the existing directory, then stack internal pages over the leaf, each with
    // one child, and rewrite the directory to cover them.
    let directory_at = i64::from_le_bytes(bytes[18..26].try_into().unwrap()) as usize;
    let mut offsets: Vec<i64> = (0..page_count as usize)
        .map(|i| {
            let slot = directory_at + i * 8;
            i64::from_le_bytes(bytes[slot..slot + 8].try_into().unwrap())
        })
        .collect();

    let digest = u64::from_le_bytes(
        bytes[page_offset(&bytes, leaf) + 28..page_offset(&bytes, leaf) + 36]
            .try_into()
            .unwrap(),
    );
    let mut file = bytes[..directory_at].to_vec();
    let mut child = leaf as i64;
    // One more level than a search can descend, counting the leaf.
    for _ in 0..64 {
        let mut node = vec![0u8; 28];
        // Internal, compact metadata, keys omitted: the payload is the child ordinal.
        node[4..8].copy_from_slice(&(1i32 | (1 << 10) | (1 << 11)).to_le_bytes());
        node[8..12].copy_from_slice(&1i32.to_le_bytes());
        node[12..20].copy_from_slice(&(-1i64).to_le_bytes());
        node[20..28].copy_from_slice(&(-1i64).to_le_bytes());
        node.extend_from_slice(&digest.to_le_bytes());
        node.extend_from_slice(&child.to_le_bytes());
        let len = node.len() as i32;
        node[0..4].copy_from_slice(&len.to_le_bytes());
        offsets.push(file.len() as i64);
        child = offsets.len() as i64 - 1;
        file.extend_from_slice(&node);
    }

    let new_directory = file.len() as i64;
    for offset in &offsets {
        file.extend_from_slice(&offset.to_le_bytes());
    }
    // Header: page count is an i32 at 14, the directory position an i64 at 18.
    file[14..18].copy_from_slice(&(offsets.len() as i32).to_le_bytes());
    file[18..26].copy_from_slice(&new_directory.to_le_bytes());
    file[root_field..root_field + 8].copy_from_slice(&child.to_le_bytes());

    let db = Database::open_bytes(file).unwrap();
    let table = db.table("t").unwrap();
    let search = table.get(&Int64Encoding::encode(0));
    let report = db.verify(Default::default()).unwrap();
    assert!(
        search.is_err(),
        "a search cannot descend sixty-four levels plus a leaf"
    );
    assert!(
        !report.is_ok(),
        "verification must not call a tree sound that a search cannot descend"
    );
}

/// A temporary file's name used to carry the table's, so a name holding a path separator
/// or longer than the file system allows made a build fail part way through sorting, on
/// input the builder had already accepted.
#[test]
fn a_table_name_does_not_decide_what_a_temporary_file_is_called() {
    for name in ["a/b".to_string(), "a".repeat(240)] {
        let mut builder = DatabaseBuilder::new()
            .page_size(4096)
            .unwrap()
            .sort_buffer(65_536);
        let table = builder
            .create_table(name.clone(), Arc::new(Int64Encoding))
            .unwrap();
        // Enough rows to spill the sort buffer to a temporary file.
        for i in 0..800i64 {
            builder
                .append(table, &Int64Encoding::encode(i), &[b'v'; 100])
                .expect("appending must not fail over what the table is called");
        }
        let bytes = builder.build_to_vec().unwrap();

        let db = Database::open_bytes(bytes).unwrap();
        let table = db.table(&name).unwrap();
        assert_eq!(table.count().unwrap(), 800);
        assert!(db.verify(Default::default()).unwrap().is_ok());
    }
}

/// A tree can change layout at a page boundary, and then neither side's own rule sees the
/// crossing: the page that keeps its keys compares keys, the page that omits them
/// compares rebuilt ones, and an order violation between the two was reported by neither.
#[test]
fn verification_reports_an_order_violation_where_the_page_layout_changes() {
    // A non-unique index keeps its key bytes here, so both leaves start out that way and
    // the second one is rewritten to omit them.
    let mut builder = DatabaseBuilder::new().page_size(256).unwrap();
    let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    builder
        .add_secondary_index(
            table,
            "by_group",
            false,
            Arc::new(Int64Encoding),
            Box::new(|_key: &[u8], value: &[u8]| Ok(value[..8].to_vec())),
        )
        .unwrap();
    for i in 0..40i64 {
        // Twenty rows to a group, so a leaf boundary falls inside one: the page after it
        // then starts on the same group with a record id that is not zero.
        let mut value = (i / 20).to_le_bytes().to_vec();
        value.extend_from_slice(&i.to_le_bytes());
        builder
            .append(table, &Int64Encoding::encode(i), &value)
            .unwrap();
    }
    let mut bytes = builder.build_to_vec().unwrap();

    let db = Database::open_bytes(bytes.clone()).unwrap();
    assert!(db.verify(Default::default()).unwrap().is_ok());
    let index_root = db.catalog().tables[0].secondaries[0].root.unwrap().get();
    drop(db);

    // The second leaf of the index.
    let mut ordinal = index_root;
    let first_leaf = loop {
        let page = page_offset(&bytes, ordinal);
        let kind_word = i32::from_le_bytes(bytes[page + 4..page + 8].try_into().unwrap());
        if kind_word & 0xFF == 0 {
            break page;
        }
        ordinal = internal_child(&bytes, page, 0);
    };
    let right = i64::from_le_bytes(bytes[first_leaf + 20..first_leaf + 28].try_into().unwrap());
    assert!(right >= 0, "the index needs two leaves");
    let second_leaf = page_offset(&bytes, right as u64);
    let kind_word = i32::from_le_bytes(bytes[second_leaf + 4..second_leaf + 8].try_into().unwrap());
    assert_eq!(
        kind_word & (1 << 11),
        0,
        "the leaves start out keeping keys"
    );

    // Rewrite it as a page that omits its keys: same digests, payloads reduced to the
    // sixteen byte reference. Every rebuilt key is then `group || 0`, which sorts below
    // the last key of the page before it, whose record id is not zero.
    let entries = entry_count(&bytes, second_leaf);
    let meta = meta_base(&bytes, second_leaf);
    let original_len =
        i32::from_le_bytes(bytes[second_leaf..second_leaf + 4].try_into().unwrap()) as usize;
    let mut values = Vec::new();
    for i in 0..entries {
        let (start, key_len) = leaf_entry_span(&bytes, second_leaf, i);
        let end = u16::from_le_bytes(
            bytes[meta + (i + 1) * 2..meta + (i + 1) * 2 + 2]
                .try_into()
                .unwrap(),
        ) as usize;
        values.push(bytes[start + key_len..second_leaf + end].to_vec());
    }

    let mut page = bytes[second_leaf..second_leaf + 28 + entries * 8].to_vec();
    page[4..8].copy_from_slice(&(kind_word | (1 << 11)).to_le_bytes());
    let payload_base = page.len() + (entries + 1) * 2;
    let mut offset = payload_base;
    for value in &values {
        page.extend_from_slice(&(offset as u16).to_le_bytes());
        offset += value.len();
    }
    page.extend_from_slice(&(offset as u16).to_le_bytes());
    for value in &values {
        page.extend_from_slice(value);
    }
    assert!(page.len() <= original_len, "the rewritten page has to fit");
    page.resize(original_len, 0);
    page[0..4].copy_from_slice(&(original_len as i32).to_le_bytes());
    bytes[second_leaf..second_leaf + original_len].copy_from_slice(&page);

    let db = Database::open_bytes(bytes).unwrap();
    let report = db.verify(Default::default()).unwrap();
    assert!(
        !report.is_ok(),
        "verification missed an order violation across a page boundary"
    );
}

/// Counting a step to the right sibling against the tree's depth turned a wide tree into
/// one that looked too deep: the cached lookup refused a key the ordinary lookup found.
#[test]
fn a_cached_lookup_does_not_count_sibling_steps_as_depth() {
    // Built with an exact digest, so the pages omit their keys, and read with an inexact
    // one, so a lookup descends conservatively and walks right across leaves.
    let mut builder = DatabaseBuilder::new().page_size(256).unwrap();
    let table = builder
        .create_table("t", Arc::new(CoarseKey { exact: true }))
        .unwrap();
    for rid in 0..40i32 {
        builder
            // Twenty rows to a source key, which is a leaf's worth: the rows for the
            // second one start only on the second leaf, so a lookup for it lands on the
            // first and has to walk right.
            .append(table, &coarse_key(rid as i64 / 20, rid), &[rid as u8])
            .unwrap();
    }
    let bytes = builder.build_to_vec().unwrap();

    // Exactly the depth this tree has, so a step to the right sibling counted as depth
    // pushes it over.
    let limits = drydb::Limits {
        max_tree_depth: 2,
        ..Default::default()
    };
    let db = OpenOptions::new()
        .register_encoding(Arc::new(CoarseKey { exact: false }))
        .unwrap()
        .limits(limits)
        .open_bytes(bytes)
        .unwrap();
    let table = db.table("t").unwrap();
    // Reading every page puts them all in the cache.
    let mut cursor = table.scan(Order::Ascending).unwrap();
    while cursor.advance().unwrap() {}
    drop(cursor);

    // A key whose run starts on an earlier leaf, so the lookup walks right to reach it.
    let key = coarse_key(1, 0);
    assert!(table.get(&key).unwrap().is_some());
    match table.get_cached(&key).unwrap() {
        drydb::CachedLookup::Value(_) => {}
        other => panic!("the cached lookup has to agree with the ordinary one: {other:?}"),
    }
}

/// Reclaiming for a new load evicted once and gave up. The first page the clock reaches
/// can be one a tree still holds, so dropping the cache's reference frees nothing and the
/// page after it is the one that would have: the same read failed and then succeeded.
#[test]
fn a_read_reclaims_past_a_page_that_is_still_held() {
    let mut builder = DatabaseBuilder::new().page_size(256).unwrap();
    let table = builder.create_table("t", Arc::new(AsciiEncoding)).unwrap();
    for i in 0..100u32 {
        builder
            .append(table, format!("{i:04}").as_bytes(), &[b'v'; 32])
            .unwrap();
    }
    let bytes = builder.build_to_vec().unwrap();

    let db = OpenOptions::new()
        .memory_budget(32_768)
        .cache_shards(1)
        .open_bytes(bytes)
        .unwrap();
    let table = db.table("t").unwrap();
    assert!(table.get(b"0000").unwrap().is_some());

    // Hold a cursor whose bound copy takes nearly all of what is left, so the next read
    // has to reclaim, starting from the root the tree itself is holding.
    let charged = db.memory_report().charged;
    // Under a marker's worth of room left, so the next read has to reclaim.
    let bound = vec![b'0'; (32_768 - charged) as usize - 300];
    let _cursor = table.range(
        Bound::Included(&bound[..]),
        Bound::Unbounded,
        Order::Ascending,
    );

    assert!(
        table.get(b"0099").unwrap().is_some(),
        "the read has to reclaim past the page the tree is holding, not stop at it"
    );
}

/// Reserving an in-flight load's marker could only reclaim from the page cache, so a
/// read failed while the memory it needed was sitting in the page directory.
#[test]
fn a_read_reclaims_from_the_page_directory_for_its_own_bookkeeping() {
    let mut builder = DatabaseBuilder::new().page_size(128).unwrap();
    let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    for i in 0..3000i64 {
        builder
            .append(table, &Int64Encoding::encode(i), &[b'v'; 60])
            .unwrap();
    }
    let bytes = builder.build_to_vec().unwrap();

    let db = OpenOptions::new()
        .memory_budget(65_536)
        // Nothing stays in the page cache, so the directory is the only place left with
        // memory to give back.
        .cache_capacity(0)
        .cache_shards(1)
        .directory_chunk_entries(1)
        .directory_chunks(100_000)
        .open_bytes(bytes)
        .unwrap();
    let table = db.table("t").unwrap();

    // Fill the directory.
    let mut cursor = table.scan(Order::Ascending).unwrap();
    while cursor.advance().unwrap() {}
    drop(cursor);
    let charged = db.memory_report().charged;
    assert!(charged > 32_768, "the directory is full");

    // Under a marker's worth of room left, so the next read has to reclaim, and the
    // directory is the only place holding anything.
    let bound = Int64Encoding::encode(0);
    let filler = vec![b'k'; (65_536 - charged) as usize - 292];
    let _held = db
        .table("t")
        .unwrap()
        .reserve(filler.len() as u64 + drydb::BUFFER_OVERHEAD)
        .expect("the filler fits");
    let _ = (bound, filler);

    assert!(
        table.get(&Int64Encoding::encode(2999)).unwrap().is_some(),
        "the read has to take memory back from the page directory"
    );
}

/// A page's declared length used to be checked only against the page directory, so a
/// length that ran into the page after it handed those bytes back as part of this page's
/// value, and verification called the file sound.
#[test]
fn a_page_that_runs_into_the_next_one_is_rejected() {
    let mut builder = DatabaseBuilder::new().page_size(256).unwrap();
    let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    builder
        .append(table, &Int64Encoding::encode(0), &[b'A'; 1024])
        .unwrap();
    builder
        .append(table, &Int64Encoding::encode(1), &[b'B'; 1024])
        .unwrap();
    let mut bytes = builder.build_to_vec().unwrap();

    let db = Database::open_bytes(bytes.clone()).unwrap();
    assert_eq!(
        db.table("t")
            .unwrap()
            .get(&Int64Encoding::encode(0))
            .unwrap()
            .unwrap()
            .len(),
        1024
    );
    assert!(db.verify(Default::default()).unwrap().is_ok());
    let pages = db.catalog().page_count;
    drop(db);

    // The first blob page, stretched over the start of the next one.
    let blob = (0..pages)
        .map(|o| page_offset(&bytes, o))
        .find(|&p| {
            let kind = i32::from_le_bytes(bytes[p + 4..p + 8].try_into().unwrap());
            kind & 0xFF == 0 && entry_count(&bytes, p) == 0
        })
        .expect("the values are on blob pages");
    let length = i32::from_le_bytes(bytes[blob..blob + 4].try_into().unwrap());
    bytes[blob..blob + 4].copy_from_slice(&(length + 32).to_le_bytes());

    let db = Database::open_bytes(bytes).unwrap();
    let table = db.table("t").unwrap();
    let err = table.get(&Int64Encoding::encode(0)).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::CorruptData);
    assert!(err.to_string().contains("into what follows it"), "{err}");
    assert!(!db.verify(Default::default()).unwrap().is_ok());
}

/// An index entry's reference has to land on a record of its own table. Pointing it at
/// the index's own value area made a lookup hand back the reference bytes instead of the
/// record, and verification saw a range that matched some entry's value and accepted it.
#[test]
fn an_index_reference_into_the_index_itself_is_rejected() {
    let mut builder = DatabaseBuilder::new().page_size(256).unwrap();
    let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    builder
        .add_secondary_index(
            table,
            "i",
            true,
            Arc::new(AsciiEncoding),
            Box::new(|_key: &[u8], _value: &[u8]| Ok(b"k".to_vec())),
        )
        .unwrap();
    builder
        .append(table, &Int64Encoding::encode(1), b"payload")
        .unwrap();
    let mut bytes = builder.build_to_vec().unwrap();

    let db = Database::open_bytes(bytes.clone()).unwrap();
    assert!(db.verify(Default::default()).unwrap().is_ok());
    let index_root = db.catalog().tables[0].secondaries[0].root.unwrap().get();
    drop(db);

    // Point the index entry at its own value, which is the reference itself.
    let page = page_offset(&bytes, index_root);
    let (start, key_len) = leaf_entry_span(&bytes, page, 0);
    let reference_at = start + key_len;
    let local = (reference_at - page) as i32;
    bytes[reference_at..reference_at + 8].copy_from_slice(&(index_root as i64).to_le_bytes());
    bytes[reference_at + 8..reference_at + 12].copy_from_slice(&local.to_le_bytes());
    bytes[reference_at + 12..reference_at + 16].copy_from_slice(&16i32.to_le_bytes());

    let db = Database::open_bytes(bytes).unwrap();
    let index = db.table("t").unwrap().index("i").unwrap();
    let mut cursor = index.lookup(b"k").unwrap();
    let value = cursor.advance().map(|more| {
        more.then(|| cursor.value().map(|v| v.as_ref().to_vec()))
            .flatten()
    });
    let report = db.verify(Default::default()).unwrap();
    assert!(
        !report.is_ok(),
        "verification accepted a reference into the index itself, and the lookup \
         returned {value:?}"
    );
}

/// A blob page says nothing about which table it belongs to, so an index reference into
/// another table's blob matched on range alone: the primary lookup returned one row's
/// value and the index lookup another's.
#[test]
fn an_index_reference_into_another_tables_blob_is_rejected() {
    let mut builder = DatabaseBuilder::new().page_size(256).unwrap();
    let t = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    builder
        .add_secondary_index(
            t,
            "i",
            true,
            Arc::new(AsciiEncoding),
            Box::new(|_key: &[u8], _value: &[u8]| Ok(b"k".to_vec())),
        )
        .unwrap();
    let foreign = builder
        .create_table("foreign", Arc::new(Int64Encoding))
        .unwrap();
    builder
        .append(t, &Int64Encoding::encode(0), &[b'A'; 1000])
        .unwrap();
    builder
        .append(foreign, &Int64Encoding::encode(0), &[b'B'; 1000])
        .unwrap();
    let mut bytes = builder.build_to_vec().unwrap();

    let db = Database::open_bytes(bytes.clone()).unwrap();
    assert!(db.verify(Default::default()).unwrap().is_ok());
    let index_root = db.catalog().tables[0].secondaries[0].root.unwrap().get();
    let pages = db.catalog().page_count;
    drop(db);

    // The index entry's reference, pointed at the other table's blob page.
    let page = page_offset(&bytes, index_root);
    let (start, key_len) = leaf_entry_span(&bytes, page, 0);
    let reference_at = start + key_len;
    let ours = i64::from_le_bytes(bytes[reference_at..reference_at + 8].try_into().unwrap()) as u64;
    let theirs = (0..pages)
        .filter(|&o| o != ours)
        .find(|&o| {
            let p = page_offset(&bytes, o);
            let kind = i32::from_le_bytes(bytes[p + 4..p + 8].try_into().unwrap());
            let len = i32::from_le_bytes(bytes[p..p + 4].try_into().unwrap()) as usize;
            kind & 0xFF == 0 && entry_count(&bytes, p) == 0 && len > 1000
        })
        .expect("the other table's value is on a blob page");
    bytes[reference_at..reference_at + 8].copy_from_slice(&(theirs as i64).to_le_bytes());

    let db = Database::open_bytes(bytes).unwrap();
    let report = db.verify(Default::default()).unwrap();
    assert!(
        !report.is_ok(),
        "verification accepted a reference into another table's blob"
    );
}

/// Widening an index key into its run of record ids turned bounds that merely meet,
/// which select nothing, into bounds that cross, which is what a reversed range looks
/// like. The same query against the primary tree answers with no rows.
#[test]
fn an_empty_range_on_a_non_unique_index_is_empty_rather_than_invalid() {
    let mut builder = DatabaseBuilder::new().page_size(256).unwrap();
    let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    builder
        .add_secondary_index(
            table,
            "by_key",
            false,
            Arc::new(Int64Encoding),
            Box::new(|key: &[u8], _value: &[u8]| Ok(key.to_vec())),
        )
        .unwrap();
    for i in 0..3i64 {
        builder
            .append(table, &Int64Encoding::encode(i), &[i as u8])
            .unwrap();
    }
    let bytes = builder.build_to_vec().unwrap();

    let db = Database::open_bytes(bytes).unwrap();
    let table = db.table("t").unwrap();
    let index = table.index("by_key").unwrap();
    let one = Int64Encoding::encode(1);
    let bounds = (Bound::Excluded(&one[..]), Bound::Excluded(&one[..]));

    assert_eq!(table.count_range(bounds.0, bounds.1).unwrap(), 0);
    assert_eq!(index.count_range(bounds.0, bounds.1).unwrap(), 0);
    for order in [Order::Ascending, Order::Descending] {
        let mut cursor = index.range(bounds.0, bounds.1, order).unwrap();
        assert!(!cursor.advance().unwrap(), "the range selects nothing");
    }

    // A range that really is reversed is still a caller's mistake.
    let two = Int64Encoding::encode(2);
    assert_eq!(
        index
            .range(
                Bound::Included(&two[..]),
                Bound::Included(&one[..]),
                Order::Ascending
            )
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidArgument
    );
}

/// A tree used to hold its own root page for as long as it lived. Once a handful of
/// tables had each taken one, a read that had succeeded a moment earlier could no longer
/// find the memory for a large value, and no amount of freeing pages got it back: the
/// roots were out of the reclaiming path's reach. They now belong to the page store,
/// which gives them up when a read needs the memory more.
#[test]
fn a_read_that_succeeded_still_succeeds_after_other_tables_are_read() {
    let mut builder = DatabaseBuilder::new().page_size(4096).unwrap();
    let big = builder
        .create_table("big", Arc::new(Int64Encoding))
        .unwrap();
    builder
        .append(big, &Int64Encoding::encode(0), &vec![b'b'; 184_320])
        .unwrap();
    for i in 0..10 {
        let t = builder
            .create_table(format!("t{i}"), Arc::new(Int64Encoding))
            .unwrap();
        builder
            .append(t, &Int64Encoding::encode(0), &vec![b's'; 3_500])
            .unwrap();
    }
    let bytes = builder.build_to_vec().unwrap();

    let db = OpenOptions::new()
        // Room for the large value with a little to spare, and nothing resident between
        // reads: whatever is still charged afterwards is charged because something is
        // holding it on purpose.
        .memory_budget(262_144)
        .cache_capacity(0)
        .cache_shards(1)
        .open_bytes(bytes)
        .unwrap();

    let key = Int64Encoding::encode(0);
    let big_table = db.table("big").unwrap();
    let first = big_table
        .get(&key)
        .expect("the large value reads on an idle budget")
        .expect("the row is there");
    assert_eq!(first.len(), 184_320);
    drop(first);

    // Each of these leaves its root page charged to the budget.
    let handles: Vec<_> = (0..10)
        .map(|i| db.table(&format!("t{i}")).unwrap())
        .collect();
    for handle in &handles {
        let value = handle.get(&key).unwrap().expect("the row is there");
        assert_eq!(value.len(), 3_500);
    }

    let again = big_table
        .get(&key)
        .expect("the large value has to still be readable")
        .expect("the row is there");
    assert_eq!(again.len(), 184_320);
}

/// Turning both passes off used to hand back an empty report, which reads exactly like a
/// file that was checked and found sound. Asking for no checking is now refused.
#[test]
fn a_verification_that_would_check_nothing_is_refused() {
    let mut builder = DatabaseBuilder::new().page_size(256).unwrap();
    let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    builder
        .append(table, &Int64Encoding::encode(0), b"v")
        .unwrap();
    let db = Database::open_bytes(builder.build_to_vec().unwrap()).unwrap();

    let nothing = VerifyOptions {
        check_pages: false,
        check_trees: false,
        ..VerifyOptions::default()
    };
    assert_eq!(
        db.verify(nothing).unwrap_err().kind(),
        ErrorKind::InvalidArgument
    );

    // Either pass on its own is still a verification.
    for (pages, trees) in [(true, false), (false, true), (true, true)] {
        let report = db
            .verify(VerifyOptions {
                check_pages: pages,
                check_trees: trees,
                ..VerifyOptions::default()
            })
            .unwrap();
        assert!(report.is_ok(), "{report:?}");
    }
}

/// Loading a page directory chunk reclaimed from the page cache alone, so a retained
/// root page it could have taken back stayed put and the lookup failed for good. The
/// directory now reclaims through the same path a page reservation uses.
#[test]
fn a_directory_chunk_can_reclaim_a_retained_root_page() {
    let mut builder = DatabaseBuilder::new().page_size(4096).unwrap();
    for (name, len) in [("a", 1024usize), ("b", 1), ("c", 15_248)] {
        let table = builder.create_table(name, Arc::new(Int64Encoding)).unwrap();
        builder
            .append(table, &Int64Encoding::encode(0), &vec![b'v'; len])
            .unwrap();
    }
    let bytes = builder.build_to_vec().unwrap();

    let db = OpenOptions::new()
        .memory_budget(32_768)
        .cache_shards(1)
        .open_bytes(bytes)
        .unwrap();
    let key = Int64Encoding::encode(0);

    let a = db.table("a").unwrap();
    assert_eq!(a.get(&key).unwrap().expect("the row is there").len(), 1024);

    // Held across the next lookup, so the budget is genuinely tight rather than merely
    // busy: what is left has to come from something that is only being kept.
    let c = db.table("c").unwrap();
    let held = c.get(&key).unwrap().expect("the row is there");
    assert_eq!(held.len(), 15_248);

    let b = db.table("b").unwrap();
    let value = b
        .get(&key)
        .expect("the lookup has to reclaim the root pages it no longer needs")
        .expect("the row is there");
    assert_eq!(value.len(), 1);
    assert_eq!(held.len(), 15_248);
}

/// Asking a filter what it needs can allocate: the zstd filter used to measure its
/// decompression context by making one, every time a database was opened, including
/// when the open was about to be refused for want of the memory. The question is now
/// reserved before it is asked, so a budget that cannot hold the filter refuses first.
#[test]
fn a_budget_too_small_for_a_filter_refuses_before_the_filter_is_asked() {
    #[derive(Debug)]
    struct CountingFilter {
        asked: Arc<AtomicU64>,
    }

    impl drydb::PageFilter for CountingFilter {
        fn id(&self) -> &str {
            "test.counting"
        }
        fn encode(&self, input: &[u8], out: &mut Vec<u8>) -> drydb::Result<()> {
            out.extend_from_slice(input);
            Ok(())
        }
        fn decode(&self, input: &[u8], out: &mut Vec<u8>) -> drydb::Result<()> {
            out.extend_from_slice(input);
            Ok(())
        }
        fn working_set_bytes(&self) -> u64 {
            self.asked.fetch_add(1, Ordering::Relaxed);
            192 * 1024
        }
        fn probe_bytes(&self) -> u64 {
            192 * 1024
        }
    }

    let asked = Arc::new(AtomicU64::new(0));
    let filter = Arc::new(CountingFilter {
        asked: Arc::clone(&asked),
    });

    let mut builder = DatabaseBuilder::new()
        .page_size(256)
        .unwrap()
        .page_filter(Arc::clone(&filter) as Arc<dyn drydb::PageFilter>);
    let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    builder
        .append(table, &Int64Encoding::encode(0), b"v")
        .unwrap();
    let bytes = builder.build_to_vec().unwrap();

    // Too small to hold what the filter needs, and too small to hold the asking.
    let err = OpenOptions::new()
        .memory_budget(32_768)
        .cache_shards(1)
        .register_filter(Arc::clone(&filter) as Arc<dyn drydb::PageFilter>)
        .open_bytes(bytes.clone())
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidArgument);
    assert_eq!(
        asked.load(Ordering::Relaxed),
        0,
        "the filter must not be asked on a budget that cannot hold the question"
    );

    // With room for both, the file opens and the filter is asked once.
    let db = OpenOptions::new()
        .memory_budget(16 << 20)
        .cache_shards(1)
        .register_filter(filter as Arc<dyn drydb::PageFilter>)
        .open_bytes(bytes)
        .unwrap();
    assert_eq!(asked.load(Ordering::Relaxed), 1);
    let table = db.table("t").unwrap();
    assert_eq!(
        table.get(&Int64Encoding::encode(0)).unwrap().as_deref(),
        Some(&b"v"[..])
    );
}

/// The zstd filter used to measure its decompression context every time one was built,
/// which is every time a compressed database is opened. It is a property of the build,
/// not of the file, so it is measured once and kept.
#[cfg(feature = "zstd")]
#[test]
fn the_zstd_filter_measures_its_working_set_once_per_process() {
    use drydb::{PageFilter, ZstdFilter};

    let first = ZstdFilter::default();
    // Either nothing has asked yet, in which case asking costs something, or something
    // has, in which case it costs nothing.
    assert!(first.probe_bytes() == 0 || first.probe_bytes() >= 128 * 1024);

    let measured = first.working_set_bytes();
    assert!(measured >= 128 * 1024, "{measured}");
    assert_eq!(
        first.probe_bytes(),
        0,
        "nothing left to measure once the figure is known"
    );

    // A second filter, and the same filter again, answer from the same measurement.
    let second = ZstdFilter::with_level(1);
    assert_eq!(second.probe_bytes(), 0);
    assert_eq!(second.working_set_bytes(), measured);
    assert_eq!(first.working_set_bytes(), measured);
}

/// Running out of budget is not a finding: the pass did not learn that the file is
/// damaged, it learned that it could not look. It used to come back inside an `Ok`
/// report as though the file were corrupt.
#[test]
fn a_verification_that_runs_out_of_budget_says_so_rather_than_blaming_the_file() {
    let mut builder = DatabaseBuilder::new().page_size(256).unwrap();
    let table = builder.create_table("t", Arc::new(AsciiEncoding)).unwrap();
    builder.append(table, b"a", &vec![b'v'; 20_000]).unwrap();
    let bytes = builder.build_to_vec().unwrap();

    // Too small to hold the value's page, which the walk has to read.
    let db = OpenOptions::new()
        .memory_budget(16 * 1024)
        .open_bytes(bytes.clone())
        .unwrap();
    let err = db
        .verify(VerifyOptions::default())
        .expect_err("a pass that cannot read the file has not checked it");
    assert_eq!(err.kind(), ErrorKind::BudgetExceeded);

    // The same file, with room to read it, is sound.
    let db = OpenOptions::new()
        .memory_budget(64 * 1024)
        .open_bytes(bytes)
        .unwrap();
    let report = db.verify(VerifyOptions::default()).unwrap();
    assert!(report.is_ok(), "{report:?}");
}

/// A name that cannot be found is the caller's, of the caller's length, and rendering it
/// whole spent more memory reporting the mistake than the lookup ever asked for.
#[test]
fn a_missing_name_is_reported_without_copying_it() {
    let mut builder = DatabaseBuilder::new().page_size(256).unwrap();
    let table = builder.create_table("t", Arc::new(AsciiEncoding)).unwrap();
    builder
        .add_secondary_index(
            table,
            "by_key",
            true,
            Arc::new(AsciiEncoding),
            Box::new(|key: &[u8], _value: &[u8]| Ok(key.to_vec())),
        )
        .unwrap();
    builder.append(table, b"a", b"v").unwrap();
    let db = Database::open_bytes(builder.build_to_vec().unwrap()).unwrap();

    let long = "n".repeat(1 << 20);
    let err = db.table(&long).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidArgument);
    assert!(
        err.to_string().len() < 200,
        "the message is {} bytes long",
        err.to_string().len()
    );

    let table = db.table("t").unwrap();
    let err = table.index(&long).unwrap_err();
    assert!(
        err.to_string().len() < 200,
        "the message is {} bytes long",
        err.to_string().len()
    );
    // The name that is there still reads in full.
    assert!(table.index("by_key").is_ok());
}

/// The same as above for the value pages a walk follows: running out of room to read one
/// is not a finding about the file.
#[test]
fn a_blob_page_that_cannot_be_read_for_want_of_budget_is_not_a_problem() {
    let mut builder = DatabaseBuilder::new().page_size(256).unwrap();
    let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    builder
        .append(table, &Int64Encoding::encode(0), &vec![b'v'; 128 * 1024])
        .unwrap();
    let bytes = builder.build_to_vec().unwrap();

    let db = OpenOptions::new()
        .memory_budget(32 * 1024)
        .cache_shards(1)
        .directory_chunk_entries(1)
        .open_bytes(bytes.clone())
        .unwrap();
    let err = db
        .verify(VerifyOptions {
            check_pages: false,
            ..VerifyOptions::default()
        })
        .expect_err("the value page does not fit, so the walk has not checked it");
    assert_eq!(err.kind(), ErrorKind::BudgetExceeded);

    let db = OpenOptions::new()
        .memory_budget(1 << 20)
        .open_bytes(bytes)
        .unwrap();
    assert!(db.verify(VerifyOptions::default()).unwrap().is_ok());
}

/// A read that fails is not evidence that the file is wrong. Verification used to record
/// one as a problem, so a sound file behind a flaky disk read as damaged.
#[test]
fn a_read_that_fails_stops_the_verification_rather_than_condemning_the_file() {
    struct Flaky {
        inner: MemorySource,
        fail: AtomicBool,
    }

    impl PageSource for Flaky {
        fn read_at(&self, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
            if self.fail.load(Ordering::Relaxed) {
                return Err(std::io::Error::from(std::io::ErrorKind::TimedOut));
            }
            self.inner.read_at(buf, offset)
        }

        fn size(&self) -> std::io::Result<u64> {
            self.inner.size()
        }
    }

    let mut builder = DatabaseBuilder::new().page_size(256).unwrap();
    let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    builder
        .append(table, &Int64Encoding::encode(0), b"v")
        .unwrap();
    let source = Arc::new(Flaky {
        inner: MemorySource::new(builder.build_to_vec().unwrap()),
        fail: AtomicBool::new(false),
    });

    let db = OpenOptions::new()
        .memory_budget(1 << 20)
        .cache_capacity(0)
        .open_source(Arc::clone(&source) as Arc<dyn PageSource>)
        .unwrap();
    assert!(db.verify(VerifyOptions::default()).unwrap().is_ok());

    source.fail.store(true, Ordering::Relaxed);
    let err = db
        .verify(VerifyOptions::default())
        .expect_err("a pass that could not read the file has not checked it");
    assert_eq!(err.kind(), ErrorKind::Io);

    // The disk comes back, and so does the answer.
    source.fail.store(false, Ordering::Relaxed);
    assert!(db.verify(VerifyOptions::default()).unwrap().is_ok());
}

/// The final count a secondary index walk takes used to fall back to the scan's own
/// figure when it could not be taken, so the two agreed because neither had run.
#[test]
fn a_count_that_could_not_be_taken_is_not_a_count_that_agreed() {
    struct FailsOnce {
        inner: MemorySource,
        reads: AtomicU64,
        fail_at: AtomicU64,
    }

    impl PageSource for FailsOnce {
        fn read_at(&self, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
            let n = self.reads.fetch_add(1, Ordering::Relaxed);
            if n == self.fail_at.load(Ordering::Relaxed) {
                return Err(std::io::Error::from(std::io::ErrorKind::TimedOut));
            }
            self.inner.read_at(buf, offset)
        }

        fn size(&self) -> std::io::Result<u64> {
            self.inner.size()
        }
    }

    let mut builder = DatabaseBuilder::new().page_size(256).unwrap();
    let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    builder
        .add_secondary_index(
            table,
            "by_key",
            true,
            Arc::new(Int64Encoding),
            Box::new(|key: &[u8], _value: &[u8]| Ok(key.to_vec())),
        )
        .unwrap();
    for i in 0..20i64 {
        builder
            .append(table, &Int64Encoding::encode(i), b"v")
            .unwrap();
    }
    let bytes = builder.build_to_vec().unwrap();

    // The count runs after the scan, so its reads are the last of the pass. Counting a
    // clean run says how many there are; failing the last one then lands inside the
    // count and nowhere else.
    let source = Arc::new(FailsOnce {
        inner: MemorySource::new(bytes.clone()),
        reads: AtomicU64::new(0),
        fail_at: AtomicU64::new(u64::MAX),
    });
    let db = OpenOptions::new()
        .memory_budget(1 << 20)
        .cache_capacity(0)
        .cache_shards(1)
        .open_source(Arc::clone(&source) as Arc<dyn PageSource>)
        .unwrap();
    source.reads.store(0, Ordering::Relaxed);
    let options = VerifyOptions {
        check_pages: false,
        ..VerifyOptions::default()
    };
    assert!(db.verify(options).unwrap().is_ok());
    let total = source.reads.load(Ordering::Relaxed);
    assert!(total > 0, "the walk read nothing");

    let source = Arc::new(FailsOnce {
        inner: MemorySource::new(bytes),
        reads: AtomicU64::new(0),
        fail_at: AtomicU64::new(u64::MAX),
    });
    let db = OpenOptions::new()
        .memory_budget(1 << 20)
        .cache_capacity(0)
        .cache_shards(1)
        .open_source(Arc::clone(&source) as Arc<dyn PageSource>)
        .unwrap();
    source.reads.store(0, Ordering::Relaxed);
    source.fail_at.store(total - 1, Ordering::Relaxed);
    let err = db
        .verify(options)
        .expect_err("the count could not be taken, so nothing was compared");
    assert_eq!(err.kind(), ErrorKind::Io);
}

/// An in-memory image is what is being read, not something read into: a small budget
/// over a large image is a normal thing to ask for and is not refused. What the budget
/// bounds is the reading, which stays inside it.
#[test]
fn an_in_memory_image_is_read_within_the_budget_whatever_its_size() {
    let mut builder = DatabaseBuilder::new().page_size(256).unwrap();
    let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    builder
        .append(table, &Int64Encoding::encode(0), &vec![b'v'; 2 << 20])
        .unwrap();
    builder
        .append(table, &Int64Encoding::encode(1), b"small")
        .unwrap();
    let bytes = builder.build_to_vec().unwrap();
    assert!(bytes.len() > 2 << 20, "an image far past the budget below");

    let db = OpenOptions::new()
        .memory_budget(65_536)
        .cache_shards(1)
        .open_bytes(bytes)
        .unwrap();
    let table = db.table("t").unwrap();

    // Reading stays inside the budget: the small row comes back, the large one does not
    // fit and says so rather than being read anyway.
    assert_eq!(
        table
            .get(&Int64Encoding::encode(1))
            .unwrap()
            .as_deref()
            .map(|v| v.len()),
        Some(5)
    );
    let err = table.get(&Int64Encoding::encode(0)).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::BudgetExceeded);
    assert!(db.charged_bytes() <= 65_536);
}

/// What an encoding writes for a key is what it reads back, so a key shown to someone
/// can be handed straight back.
#[test]
fn every_encoding_reads_back_what_it_prints() {
    let cases: Vec<(Arc<dyn drydb::KeyEncoding>, Vec<u8>)> = vec![
        (Arc::new(Int64Encoding), Int64Encoding::encode(-7).to_vec()),
        (
            Arc::new(Int64Encoding),
            Int64Encoding::encode(i64::MIN).to_vec(),
        ),
        (Arc::new(AsciiEncoding), b"a key".to_vec()),
        // Bytes no text can carry: rendering them lossily printed different keys the
        // same way, and neither could be handed back.
        (Arc::new(AsciiEncoding), vec![0xff]),
        (Arc::new(AsciiEncoding), vec![0xef, 0xbf, 0xbd]),
        (Arc::new(AsciiEncoding), (0..=255u8).collect()),
        (Arc::new(AsciiEncoding), br"back\slash".to_vec()),
        (
            Arc::new(drydb::Uuidv7Encoding),
            (0..16u8).map(|b| b.wrapping_mul(17)).collect(),
        ),
        (Arc::new(UlidEncoding), (0..16u8).collect()),
    ];
    for (encoding, key) in cases {
        let printed = encoding.format_key(&key);
        let read_back = encoding
            .parse_key(&printed)
            .unwrap_or_else(|e| panic!("`{printed}` from `{}`: {e}", encoding.id()));
        assert_eq!(read_back, key, "`{}` printed `{printed}`", encoding.id());
    }
}

/// Two keys that differ have to print differently, or the one printed cannot be the one
/// looked up. Rendering them as lossy text made every byte it could not read the same
/// character.
#[test]
fn keys_that_differ_print_differently() {
    let mut builder = DatabaseBuilder::new().page_size(256).unwrap();
    let table = builder.create_table("t", Arc::new(AsciiEncoding)).unwrap();
    builder
        .append(table, &[0xef, 0xbf, 0xbd], b"replacement")
        .unwrap();
    builder.append(table, &[0xff], b"binary").unwrap();
    let db = Database::open_bytes(builder.build_to_vec().unwrap()).unwrap();
    let table = db.table("t").unwrap();
    let encoding = table.key_encoding().clone();

    let printed: Vec<String> = [vec![0xef, 0xbf, 0xbd], vec![0xffu8]]
        .iter()
        .map(|key| encoding.format_key(key))
        .collect();
    assert_ne!(printed[0], printed[1], "{printed:?}");

    // And each one reads back as the key it came from.
    for (key, text) in [
        (vec![0xef, 0xbf, 0xbd], &printed[0]),
        (vec![0xffu8], &printed[1]),
    ] {
        let read_back = encoding.parse_key(text).unwrap();
        assert_eq!(read_back, key, "`{text}`");
        assert!(table.get(&read_back).unwrap().is_some(), "`{text}`");
    }
}

/// A bound of the wrong length is the caller's mistake, not damage to the file. It used
/// to be compared before it was checked, and what the encoding made of the bytes came
/// back as corrupt data.
#[test]
fn a_bound_of_the_wrong_length_is_an_argument_error() {
    let mut builder = DatabaseBuilder::new().page_size(256).unwrap();
    let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    builder
        .append(table, &Int64Encoding::encode(0), b"v")
        .unwrap();
    let db = Database::open_bytes(builder.build_to_vec().unwrap()).unwrap();
    let table = db.table("t").unwrap();

    let short = [0u8; 7];
    let full = [0u8; 8];
    // The same mistake through every door that takes bounds.
    assert_eq!(
        table
            .range(
                Bound::Included(&short[..]),
                Bound::Included(&full[..]),
                Order::Ascending
            )
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidArgument
    );
    assert_eq!(
        table
            .count_range(Bound::Included(&short[..]), Bound::Included(&full[..]))
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidArgument
    );
    // Which is what a key of that length gets from `get`.
    assert_eq!(
        table.get(&short).unwrap_err().kind(),
        ErrorKind::InvalidArgument
    );
}

/// A leaf chain that stops before the tree does used to end a scan quietly: the rows
/// past the break were simply not there, and the count agreed with the scan because both
/// stopped at the same place. Part of an answer given as the whole of one.
#[test]
fn a_leaf_chain_that_stops_early_is_reported_rather_than_obeyed() {
    let mut builder = DatabaseBuilder::new().page_size(256).unwrap();
    let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    for i in 0..40i64 {
        builder
            .append(table, &Int64Encoding::encode(i), &Int64Encoding::encode(i))
            .unwrap();
    }
    let mut bytes = builder.build_to_vec().unwrap();

    // The first leaf's link to the next one, cut.
    let db = Database::open_bytes(bytes.clone()).unwrap();
    let table = db.table("t").unwrap();
    let whole: Vec<i64> = {
        let mut cursor = table.scan(Order::Ascending).unwrap();
        let mut keys = Vec::new();
        while cursor.advance().unwrap() {
            keys.push(i64::from_le_bytes(
                cursor.current().unwrap().key().try_into().unwrap(),
            ));
        }
        keys
    };
    assert_eq!(whole.len(), 40, "the file is sound to start with");
    let first_leaf = {
        let mut cursor = table.scan(Order::Ascending).unwrap();
        cursor.advance().unwrap();
        cursor.current().unwrap().page().get()
    };
    drop(db);

    let at = page_offset(&bytes, first_leaf) + 20;
    bytes[at..at + 8].copy_from_slice(&(-1i64).to_le_bytes());

    let db = Database::open_bytes(bytes).unwrap();
    let table = db.table("t").unwrap();

    // The scan says what is wrong rather than stopping where the chain does.
    let mut cursor = table.scan(Order::Ascending).unwrap();
    let mut seen = 0;
    let outcome = loop {
        match cursor.advance() {
            Ok(true) => seen += 1,
            Ok(false) => break Ok(seen),
            Err(e) => break Err(e),
        }
    };
    let err = outcome.expect_err("the chain stops before the tree does");
    assert_eq!(err.kind(), ErrorKind::CorruptData);
    assert!(seen < 40, "it stopped early, which is the point");

    // And so does counting, which walks the same chain.
    let err = table.count().expect_err("the same break, the same answer");
    assert_eq!(err.kind(), ErrorKind::CorruptData);

    // Verification finds it too, as it did before.
    let report = db.verify(VerifyOptions::default()).unwrap();
    assert!(!report.is_ok(), "{report:?}");
}

/// Forty text keys over several leaves. Text keys leave room between them, so a search
/// can be aimed between the last key of one leaf and the first key of the next. The
/// descent then lands on the earlier leaf and reaches the bound by stepping along the
/// sibling chain, which is the step the tests below break.
fn build_colliding() -> Vec<u8> {
    let mut builder = DatabaseBuilder::new().page_size(256).unwrap();
    let table = builder.create_table("t", Arc::new(AsciiEncoding)).unwrap();
    for i in 0..40u32 {
        builder
            .append(table, format!("commonpre{i:03}").as_bytes(), b"v")
            .unwrap();
    }
    builder.build_to_vec().unwrap()
}

/// The first two leaf pages, with the last key of the first and the first key of the
/// second.
fn first_two_leaves(bytes: &[u8]) -> (u64, u64, String, String) {
    let db = Database::open_bytes(bytes.to_vec()).unwrap();
    let table = db.table("t").unwrap();
    let mut cursor = table.scan(Order::Ascending).unwrap();
    assert!(cursor.advance().unwrap());
    let first_leaf = cursor.current().unwrap().page().get();
    let mut last = String::from_utf8(cursor.current().unwrap().key().to_vec()).unwrap();
    loop {
        assert!(cursor.advance().unwrap(), "the file has more than one leaf");
        let entry = cursor.current().unwrap();
        let key = String::from_utf8(entry.key().to_vec()).unwrap();
        if entry.page().get() != first_leaf {
            return (first_leaf, entry.page().get(), last, key);
        }
        last = key;
    }
}

/// A scan reports a chain that stops early, but a range does not begin with a scan: it
/// searches for its first entry, and that search walks the same chain. The walk used to
/// end quietly when the link was gone, so the range came back empty and the count
/// agreed. Both answers described a file the search had never read past.
#[test]
fn a_break_found_while_seeking_the_start_of_a_range_is_reported() {
    let mut bytes = build_colliding();
    let (first_leaf, _, last_key, _) = first_two_leaves(&bytes);

    // A bound between the two leaves: the descent lands on the first one, finds nothing
    // at or above the bound, and steps right.
    let past = format!("{last_key}z");
    let bound = past.as_bytes();
    let count = |bytes: Vec<u8>| -> Result<u64, ErrorKind> {
        let db = Database::open_bytes(bytes).unwrap();
        let table = db.table("t").unwrap();
        let mut cursor = table
            .range(Bound::Included(bound), Bound::Unbounded, Order::Ascending)
            .unwrap();
        let mut seen = 0;
        loop {
            match cursor.advance() {
                Ok(true) => seen += 1,
                Ok(false) => break Ok(seen),
                Err(e) => break Err(e.kind()),
            }
        }
    };
    assert_eq!(
        count(bytes.clone()),
        Ok(31),
        "the file is sound to start with"
    );

    let at = page_offset(&bytes, first_leaf) + 20;
    bytes[at..at + 8].copy_from_slice(&(-1i64).to_le_bytes());

    assert_eq!(count(bytes.clone()), Err(ErrorKind::CorruptData));

    let db = Database::open_bytes(bytes).unwrap();
    let table = db.table("t").unwrap();
    assert_eq!(
        table
            .count_range(Bound::Included(bound), Bound::Unbounded)
            .unwrap_err()
            .kind(),
        ErrorKind::CorruptData,
        "counting takes the same search to the same break"
    );
}

/// The descending counterpart. A descending range starts at the entry below its upper
/// bound, and when the bound lands at the start of a leaf that entry is on the leaf to
/// the left. A missing link there used to mean nothing was below the bound at all.
#[test]
fn a_break_found_while_seeking_the_start_of_a_descending_range_is_reported() {
    let mut bytes = build_colliding();
    let (_, second_leaf, _, second_leaf_key) = first_two_leaves(&bytes);
    let bound = second_leaf_key.as_bytes();
    let count = |bytes: Vec<u8>| -> Result<u64, ErrorKind> {
        let db = Database::open_bytes(bytes).unwrap();
        let table = db.table("t").unwrap();
        let mut cursor = table
            .range(Bound::Unbounded, Bound::Excluded(bound), Order::Descending)
            .unwrap();
        let mut seen = 0;
        loop {
            match cursor.advance() {
                Ok(true) => seen += 1,
                Ok(false) => break Ok(seen),
                Err(e) => break Err(e.kind()),
            }
        }
    };
    assert_eq!(
        count(bytes.clone()),
        Ok(9),
        "the file is sound to start with"
    );

    // The second leaf's link back to the first one, cut.
    let at = page_offset(&bytes, second_leaf) + 12;
    bytes[at..at + 8].copy_from_slice(&(-1i64).to_le_bytes());

    assert_eq!(count(bytes), Err(ErrorKind::CorruptData));
}

/// Seeking a blob past its end used to move the position to the end instead, so a
/// relative seek afterwards counted from the end rather than from where the caller had
/// put the position, and read bytes from somewhere else. A file keeps the position it
/// is given and reads nothing there.
#[test]
fn a_blob_seek_past_the_end_keeps_the_position_it_was_given() {
    use std::io::{Read, Seek, SeekFrom};

    let mut builder = DatabaseBuilder::new().page_size(512).unwrap();
    let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    let value: Vec<u8> = (0..4096u32).map(|i| i as u8).collect();
    builder
        .append(table, &Int64Encoding::encode(1), &value)
        .unwrap();
    let bytes = builder.build_to_vec().unwrap();

    let db = Database::open_bytes(bytes).unwrap();
    let table = db.table("t").unwrap();
    let mut reader = table
        .blob_reader(&Int64Encoding::encode(1))
        .unwrap()
        .unwrap();
    let len = reader.len();
    assert_eq!(len, 4096);

    assert_eq!(reader.seek(SeekFrom::Start(len + 100)).unwrap(), len + 100);
    let mut buf = [0u8; 8];
    assert_eq!(
        reader.read(&mut buf).unwrap(),
        0,
        "there is nothing to read past the end"
    );

    // Counting back from there lands 100 bytes past the end, not 10 bytes before it.
    assert_eq!(reader.seek(SeekFrom::Current(-10)).unwrap(), len + 90);
    assert_eq!(reader.read(&mut buf).unwrap(), 0);

    // Back inside the value, the bytes are the ones stored there.
    assert_eq!(reader.seek(SeekFrom::Start(len - 8)).unwrap(), len - 8);
    reader.read_exact(&mut buf).unwrap();
    assert_eq!(buf, value[value.len() - 8..]);

    // Before the start is still an error, and so is a position no offset can hold.
    assert_eq!(
        reader.seek(SeekFrom::Start(0)).unwrap(),
        0,
        "the position is where it was put"
    );
    assert_eq!(
        reader.seek(SeekFrom::Current(-1)).unwrap_err().kind(),
        std::io::ErrorKind::InvalidInput
    );
    assert_eq!(
        reader.seek(SeekFrom::Start(u64::MAX)).unwrap(),
        u64::MAX,
        "the largest offset there is"
    );
    assert_eq!(
        reader.seek(SeekFrom::Current(1)).unwrap_err().kind(),
        std::io::ErrorKind::InvalidInput
    );
}

/// A builder setting made after a table was declared used to govern only the tables
/// declared after it: the table already there kept the settings it was born with. A
/// temporary directory set that way was ignored, and so was the sort buffer.
#[test]
fn a_builder_setting_reaches_the_tables_already_declared() {
    let dir = std::env::temp_dir().join(format!("drydb-late-setting-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let mut builder = DatabaseBuilder::new().page_size(256).unwrap();
    let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    // Set after the table exists, and pointing at a directory that is not there: the
    // build has to try it and fail, rather than spilling somewhere else.
    builder = builder.temp_dir(&dir).sort_buffer(64 * 1024);
    // The sorter spills while rows are appended, so that is where it finds out.
    let mut appended = 0i64;
    let outcome = loop {
        if appended == 20_000 {
            break Ok(());
        }
        match builder.append(
            table,
            &Int64Encoding::encode(appended),
            &Int64Encoding::encode(appended),
        ) {
            Ok(()) => appended += 1,
            Err(e) => break Err(e),
        }
    };
    let err = outcome.expect_err("the sorter has nowhere to spill to");
    assert_eq!(err.kind(), ErrorKind::Io);
    assert!(
        appended > 0 && appended < 20_000,
        "it filled the buffer and then tried to spill: {appended} rows in"
    );

    // With the directory there, the same build works and spills into it.
    std::fs::create_dir_all(&dir).unwrap();
    let mut builder = DatabaseBuilder::new().page_size(256).unwrap();
    let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    builder = builder.temp_dir(&dir).sort_buffer(64 * 1024);
    for i in 0..20_000i64 {
        builder
            .append(table, &Int64Encoding::encode(i), &Int64Encoding::encode(i))
            .unwrap();
    }
    let bytes = builder.build_to_vec().unwrap();
    let db = Database::open_bytes(bytes).unwrap();
    assert_eq!(db.table("t").unwrap().count().unwrap(), 20_000);

    let _ = std::fs::remove_dir_all(&dir);
}

/// Sorting reports the first key comparison that failed. Looking in the cell that holds
/// it took the error out and put nothing back, so a second failure found the cell empty
/// and stored itself, a third took that one out again, and an even number of failures
/// left the cell empty. The sort then looked as though it had worked, and because the
/// comparisons that followed agreed with each other the build went on to write a file.
#[test]
fn a_key_comparison_that_fails_stops_the_build_however_often_it_fails() {
    /// Fails its first `failures` comparisons and orders the rest by their bytes.
    #[derive(Debug)]
    struct FlakyOrder {
        left: AtomicU64,
    }

    impl drydb::KeyEncoding for FlakyOrder {
        fn id(&self) -> &str {
            "test.flaky"
        }
        fn compare(&self, a: &[u8], b: &[u8]) -> drydb::Result<std::cmp::Ordering> {
            if self
                .left
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
                .is_ok()
            {
                return Err(drydb::Error::new(
                    ErrorKind::Unsupported,
                    "this comparison cannot be made",
                ));
            }
            Ok(a.cmp(b))
        }
        fn digest(&self, key: &[u8]) -> drydb::Result<u64> {
            Ok(key.first().copied().unwrap_or(0) as u64)
        }
    }

    // One failure or two: the second used to displace the first and leave nothing to
    // report, and everything after it compared cleanly, so the build finished.
    for failures in 1..=4u64 {
        let encoding = Arc::new(FlakyOrder {
            left: AtomicU64::new(failures),
        });
        let mut builder = DatabaseBuilder::new().page_size(256).unwrap();
        let table = builder.create_table("t", encoding).unwrap();
        for i in 0..8u8 {
            builder.append(table, &[i], b"v").unwrap();
        }
        let err = builder
            .build_to_vec()
            .expect_err("a comparison the encoding refused cannot be forgotten");
        assert_eq!(err.kind(), ErrorKind::Unsupported, "{failures} failures");
    }
}

/// A chain that skips a leaf reads as a shorter table. The pages it does visit are in
/// order and it ends where the tree ends, so neither the order check nor the check that
/// the chain reaches the last leaf notices; the scan and the count agreed on a number
/// that left twelve rows out. The chain is linked both ways, so a step now looks at the
/// link it came by.
#[test]
fn a_leaf_chain_that_skips_a_page_is_reported_rather_than_obeyed() {
    let mut builder = DatabaseBuilder::new().page_size(256).unwrap();
    let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    for i in 0..40i64 {
        builder
            .append(table, &Int64Encoding::encode(i), &Int64Encoding::encode(i))
            .unwrap();
    }
    let mut bytes = builder.build_to_vec().unwrap();

    // The leaf pages, in the order the chain visits them.
    let db = Database::open_bytes(bytes.clone()).unwrap();
    let table = db.table("t").unwrap();
    let mut leaves: Vec<u64> = Vec::new();
    let mut cursor = table.scan(Order::Ascending).unwrap();
    while cursor.advance().unwrap() {
        let page = cursor.current().unwrap().page().get();
        if leaves.last() != Some(&page) {
            leaves.push(page);
        }
    }
    drop(cursor);
    assert!(leaves.len() >= 4, "{leaves:?}");
    assert_eq!(
        table.count().unwrap(),
        40,
        "the file is sound to start with"
    );
    drop(db);

    // The first leaf now points past the second, which is never visited.
    let at = page_offset(&bytes, leaves[0]) + 20;
    bytes[at..at + 8].copy_from_slice(&(leaves[2] as i64).to_le_bytes());

    let db = Database::open_bytes(bytes).unwrap();
    let table = db.table("t").unwrap();

    let mut cursor = table.scan(Order::Ascending).unwrap();
    let mut seen = 0;
    let outcome = loop {
        match cursor.advance() {
            Ok(true) => seen += 1,
            Ok(false) => break Ok(seen),
            Err(e) => break Err(e),
        }
    };
    let err = outcome.expect_err("the chain steps over a leaf");
    assert_eq!(err.kind(), ErrorKind::CorruptData);
    assert!(seen < 40, "it stopped where the chain jumped");

    assert_eq!(
        table.count().unwrap_err().kind(),
        ErrorKind::CorruptData,
        "counting walks the same chain"
    );

    // Descending meets the same break from the other side: the page before the one it
    // steps into does not point at it.
    let mut cursor = table.scan(Order::Descending).unwrap();
    let outcome = loop {
        match cursor.advance() {
            Ok(true) => {}
            Ok(false) => break Ok(()),
            Err(e) => break Err(e),
        }
    };
    assert_eq!(
        outcome.expect_err("the same break").kind(),
        ErrorKind::CorruptData
    );

    let report = db.verify(VerifyOptions::default()).unwrap();
    assert!(!report.is_ok(), "{report:?}");
}

/// The verification report is memory the caller keeps, and the pass reserved it while it
/// built it. The reservation was a local, released as the report was handed over, so a
/// caller holding reports held memory the budget had been told was free.
#[test]
fn a_verification_report_holds_the_reservation_that_paid_for_it() {
    let mut bytes = build_i64(200, 256, 8);

    // Damage, so the report has problems in it to pay for.
    let db = Database::open_bytes(bytes.clone()).unwrap();
    let table = db.table("t").unwrap();
    let mut cursor = table.scan(Order::Ascending).unwrap();
    cursor.advance().unwrap();
    let leaf = cursor.current().unwrap().page().get();
    drop(cursor);
    drop(db);
    let at = page_offset(&bytes, leaf) + 20;
    bytes[at..at + 8].copy_from_slice(&(-1i64).to_le_bytes());

    let db = Database::open_bytes(bytes).unwrap();
    let report = db.verify(VerifyOptions::default()).unwrap();
    assert!(!report.is_ok(), "the file is damaged");
    assert!(!report.problems.is_empty());

    // Cached pages are charged too, so the comparison is against what is charged while
    // the report is held, not against what was charged before the pass ran.
    let held = db.charged_bytes();
    drop(report);
    let released = db.charged_bytes();
    assert!(
        released < held,
        "holding the report charged {held} bytes and dropping it left {released}"
    );
}

/// What a scan costs, which the README describes: the leaf it is on, and the whole of a
/// value stored on its own page once it steps onto that row. Counting reads no value,
/// and `BlobReader` reads one chunk at a time, so both fit where the scan does not.
#[test]
fn a_scan_over_a_value_on_its_own_page_needs_room_for_the_value() {
    let mut builder = DatabaseBuilder::new().page_size(4096).unwrap();
    let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    builder
        .append(table, &Int64Encoding::encode(2), &vec![b'x'; 204_800])
        .unwrap();
    let bytes = builder.build_to_vec().unwrap();

    let db = OpenOptions::new()
        .memory_budget(64 * 1024)
        .open_bytes(bytes)
        .unwrap();
    let table = db.table("t").unwrap();

    assert_eq!(table.count().unwrap(), 1, "counting reads no value");

    let mut cursor = table.scan(Order::Ascending).unwrap();
    let err = cursor
        .advance()
        .expect_err("the value does not fit in the budget");
    assert_eq!(err.kind(), ErrorKind::BudgetExceeded);

    let reader = table
        .blob_reader(&Int64Encoding::encode(2))
        .unwrap()
        .expect("the value is on its own page");
    let mut out = Vec::new();
    assert_eq!(reader.copy_to(&mut out, 4096).unwrap(), 204_800);
}
