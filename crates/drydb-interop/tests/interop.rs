//! C# ↔ Rust interop.
//!
//! Requires the .NET SDK and the pinned upstream checkout
//! (`tests/interop/fetch-upstream.sh`). Without them the test reports what is missing
//! and returns; set `DRYDB_INTEROP=1` to make a missing prerequisite a failure instead,
//! which is what CI does.
//!
//! For every fixture the harness builds the database twice, once with each
//! implementation, and dumps each file with each implementation. Comparing the four
//! dumps separates the three claims that are easy to conflate: that Rust reads C# files,
//! that C# reads Rust files, and that both agree on what a query returns.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use drydb_interop::*;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

fn have_dotnet() -> bool {
    Command::new("dotnet")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[derive(Clone)]
struct Oracle {
    dll: PathBuf,
}

impl Oracle {
    /// Builds the oracle once per process.
    ///
    /// Every test in this binary needs it, the harness runs them in parallel, and two
    /// `dotnet build` runs sharing one output directory race over the same files.
    fn shared() -> Result<Oracle, String> {
        static ORACLE: std::sync::OnceLock<Result<Oracle, String>> = std::sync::OnceLock::new();
        ORACLE.get_or_init(Oracle::build).clone()
    }

    fn build() -> Result<Oracle, String> {
        let root = repo_root();
        let project = root.join("tests/interop/DryDbOracle/DryDbOracle.csproj");
        // Each test binary builds into its own directory: `cargo test` runs the
        // binaries in parallel, and two `dotnet build` runs sharing one output
        // directory race over the same files.
        let out = root.join("tests/interop/work/oracle-interop");
        let status = Command::new("dotnet")
            .args(["build", "-c", "Release", "-v", "quiet", "--nologo"])
            .arg(&project)
            .arg("-o")
            .arg(&out)
            .status()
            .map_err(|e| format!("cannot run dotnet build: {e}"))?;
        if !status.success() {
            return Err("dotnet build of the oracle failed".to_string());
        }
        Ok(Oracle {
            dll: out.join("DryDbOracle.dll"),
        })
    }

    /// Runs the oracle, giving up after `limit`.
    ///
    /// Used where upstream is expected not to finish; without a bound the test would
    /// hang instead of failing.
    fn run_bounded(&self, args: &[&Path], limit: Duration) -> Result<bool, String> {
        let mut child = Command::new("dotnet")
            .arg(&self.dll)
            .args(args.iter().map(|p| p.as_os_str()))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| format!("cannot run the oracle: {e}"))?;
        let deadline = std::time::Instant::now() + limit;
        loop {
            match child
                .try_wait()
                .map_err(|e| format!("cannot wait for the oracle: {e}"))?
            {
                Some(_) => return Ok(true),
                None if std::time::Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Ok(false);
                }
                None => std::thread::sleep(Duration::from_millis(50)),
            }
        }
    }

    fn run(&self, args: &[&Path]) -> Result<(), String> {
        let output = Command::new("dotnet")
            .arg(&self.dll)
            .args(args.iter().map(|p| p.as_os_str()))
            .output()
            .map_err(|e| format!("cannot run the oracle: {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "oracle failed ({}):\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        Ok(())
    }

    fn build_db(&self, spec: &Path, out: &Path) -> Result<(), String> {
        self.run(&[Path::new("build"), spec, out])
    }

    fn dump(&self, spec: &Path, db: &Path, out: &Path) -> Result<(), String> {
        self.run(&[Path::new("dump"), spec, db, out])
    }
}

/// A small deterministic generator, so fixtures are reproducible without a dependency.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(seed | 1)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 17) ^ (self.0 >> 33)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n.max(1)
    }
}

fn row(key: Vec<u8>, value: Vec<u8>) -> Row {
    Row {
        k: b64(&key),
        v: b64(&value),
    }
}

fn point(table: &str, key: &[u8]) -> PointQuery {
    PointQuery {
        table: table.to_string(),
        key: b64(key),
    }
}

fn range(
    table: &str,
    lower: Option<&[u8]>,
    upper: Option<&[u8]>,
    lower_exclusive: bool,
    upper_exclusive: bool,
    order: &str,
) -> RangeQuery {
    RangeQuery {
        table: table.to_string(),
        lower: lower.map(b64),
        upper: upper.map(b64),
        lower_exclusive,
        upper_exclusive,
        order: order.to_string(),
    }
}

fn index_range(
    table: &str,
    index: &str,
    lower: &[u8],
    upper: &[u8],
    lower_exclusive: bool,
    upper_exclusive: bool,
    order: &str,
) -> IndexRangeQuery {
    IndexRangeQuery {
        table: table.to_string(),
        index: index.to_string(),
        lower: Some(b64(lower)),
        upper: Some(b64(upper)),
        lower_exclusive,
        upper_exclusive,
        order: order.to_string(),
    }
}

/// Builds the bound combinations both implementations are asked about.
fn bound_queries(table: &str, keys: &[Vec<u8>]) -> (Vec<RangeQuery>, Vec<RangeQuery>) {
    let mut ranges = Vec::new();
    let mut counts = Vec::new();
    if keys.is_empty() {
        return (ranges, counts);
    }
    let picks = [0usize, 1, keys.len() / 3, keys.len() / 2, keys.len() - 1];
    for &lo in &picks {
        for &hi in &picks {
            if lo > hi {
                continue;
            }
            for lo_ex in [false, true] {
                for hi_ex in [false, true] {
                    for order in ["asc", "desc"] {
                        ranges.push(range(
                            table,
                            Some(&keys[lo]),
                            Some(&keys[hi]),
                            lo_ex,
                            hi_ex,
                            order,
                        ));
                    }
                    counts.push(range(
                        table,
                        Some(&keys[lo]),
                        Some(&keys[hi]),
                        lo_ex,
                        hi_ex,
                        "asc",
                    ));
                }
            }
        }
    }
    // Open ends, both directions.
    for order in ["asc", "desc"] {
        ranges.push(range(table, None, None, false, false, order));
        ranges.push(range(
            table,
            Some(&keys[keys.len() / 2]),
            None,
            false,
            false,
            order,
        ));
        ranges.push(range(
            table,
            None,
            Some(&keys[keys.len() / 2]),
            false,
            true,
            order,
        ));
    }
    counts.push(range(table, None, None, false, false, "asc"));
    (ranges, counts)
}

fn i64_fixture(
    name: &str,
    page_size: usize,
    eytzinger: bool,
    rows: usize,
    value_len: usize,
    filter: Option<String>,
) -> (String, Spec) {
    let mut rng = Rng::new(0xD1CE_0000 + rows as u64);
    let mut keys: Vec<i64> = Vec::new();
    let mut spec_rows = Vec::new();
    let mut seen = std::collections::HashSet::new();
    while keys.len() < rows {
        let key = rng.next_u64() as i64 / 3;
        if !seen.insert(key) {
            continue;
        }
        keys.push(key);
        let value: Vec<u8> = (0..value_len)
            .map(|i| (key as u8).wrapping_add(i as u8))
            .collect();
        spec_rows.push(row(key.to_le_bytes().to_vec(), value));
    }
    // Extremes, to pin down digest sign handling.
    for key in [i64::MIN, i64::MAX, 0, -1, 1] {
        if seen.insert(key) {
            keys.push(key);
            spec_rows.push(row(key.to_le_bytes().to_vec(), vec![0xAB; value_len]));
        }
    }

    let mut sorted = keys.clone();
    sorted.sort_unstable();
    let key_bytes: Vec<Vec<u8>> = sorted.iter().map(|k| k.to_le_bytes().to_vec()).collect();

    let (ranges, counts) = bound_queries("items", &key_bytes);
    let mut points: Vec<PointQuery> = key_bytes.iter().map(|k| point("items", k)).collect();
    for miss in [7_777_777i64, -7_777_777, i64::MIN + 1, i64::MAX - 1] {
        if !seen.contains(&miss) {
            points.push(point("items", &miss.to_le_bytes()));
        }
    }

    (
        name.to_string(),
        Spec {
            page_size,
            eytzinger,
            filter,
            tables: vec![TableSpec {
                name: "items".to_string(),
                encoding: "i64".to_string(),
                rows: spec_rows,
                indexes: Vec::new(),
            }],
            queries: Queries {
                points,
                ranges,
                counts,
                ..Default::default()
            },
        },
    )
}

/// Nine bytes each, sorted, so the ascii digest of an index key is not disturbed by the
/// record id upstream appends to it.
const CITY_CODES: [&str; 4] = ["BER-CITY1", "LON-CITY2", "NYC-CITY3", "TYO-CITY4"];

fn ascii_fixture(
    name: &str,
    page_size: usize,
    eytzinger: bool,
    rows: usize,
    filter: Option<String>,
) -> (String, Spec) {
    let mut rng = Rng::new(0xA5C1_1000 + rows as u64);
    let mut spec_rows = Vec::new();
    let mut keys: Vec<Vec<u8>> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    // A mix of short keys, long keys and keys sharing an eight byte prefix, which is
    // exactly where the ascii digest collides.
    while keys.len() < rows {
        let shape = rng.below(3);
        let key = match shape {
            0 => format!("k{:04}", rng.below(10_000)),
            1 => format!("commonprefix-{:06}", rng.below(1_000_000)),
            _ => {
                let len = 1 + rng.below(20) as usize;
                (0..len)
                    .map(|_| (b'a' + rng.below(26) as u8) as char)
                    .collect()
            }
        };
        if !seen.insert(key.clone()) {
            continue;
        }
        keys.push(key.clone().into_bytes());
        // Index keys are nine bytes on purpose: upstream computes a non-unique index's
        // page digests over `source||record_id` while its reader computes them over the
        // source key alone, so the two only agree once the source key fills the eight
        // digest bytes. The short-key case is covered by its own test below.
        let city = CITY_CODES[rng.below(4) as usize];
        spec_rows.push(row(
            key.clone().into_bytes(),
            format!("{city}|{key}").into_bytes(),
        ));
    }

    let mut sorted = keys.clone();
    sorted.sort();
    let (ranges, counts) = bound_queries("people", &sorted);
    let points: Vec<PointQuery> = sorted
        .iter()
        .take(40)
        .map(|k| point("people", k))
        .chain([point("people", b"zzzzz-missing"), point("people", b"a")])
        .collect();

    let index_lookups = CITY_CODES
        .iter()
        .chain(["XXXXXXXXX"].iter())
        .map(|city| IndexLookup {
            table: "people".to_string(),
            index: "by_city".to_string(),
            key: b64(city.as_bytes()),
        })
        .collect();

    let mut index_ranges = Vec::new();
    for order in ["asc", "desc"] {
        // Inclusive only on the non-unique index: upstream applies an exclusive bound to
        // the composite `(key, record_id)` it builds, so "everything above k" still
        // returns every row for k except the first. That divergence has its own test.
        index_ranges.push(index_range(
            "people",
            "by_city",
            CITY_CODES[0].as_bytes(),
            CITY_CODES[2].as_bytes(),
            false,
            false,
            order,
        ));
        // The unique index has no composite key, so every bound combination agrees.
        for (lo_ex, hi_ex) in [(false, false), (true, false), (false, true), (true, true)] {
            index_ranges.push(index_range(
                "people",
                "by_key",
                &sorted[1],
                &sorted[sorted.len() - 2],
                lo_ex,
                hi_ex,
                order,
            ));
        }
    }

    (
        name.to_string(),
        Spec {
            page_size,
            eytzinger,
            filter,
            tables: vec![TableSpec {
                name: "people".to_string(),
                encoding: "ascii".to_string(),
                rows: spec_rows,
                indexes: vec![
                    IndexSpec {
                        name: "by_city".to_string(),
                        unique: false,
                        encoding: "ascii".to_string(),
                        key_from: "value_prefix:9".to_string(),
                    },
                    IndexSpec {
                        name: "by_key".to_string(),
                        unique: true,
                        encoding: "ascii".to_string(),
                        key_from: "key".to_string(),
                    },
                ],
            }],
            queries: Queries {
                points,
                ranges,
                counts,
                index_lookups,
                index_ranges,
            },
        },
    )
}

fn overflow_fixture() -> (String, Spec) {
    let mut spec_rows = Vec::new();
    let sizes = [0usize, 1, 7, 200, 1000, 65_534, 70_000, 250_000];
    for (i, size) in sizes.iter().enumerate() {
        let value: Vec<u8> = (0..*size).map(|n| ((n * 7 + i) % 251) as u8).collect();
        spec_rows.push(row((i as i64).to_le_bytes().to_vec(), value));
    }
    let keys: Vec<Vec<u8>> = (0..sizes.len())
        .map(|i| (i as i64).to_le_bytes().to_vec())
        .collect();
    let (ranges, counts) = bound_queries("blobs", &keys);
    let points = keys.iter().map(|k| point("blobs", k)).collect();

    (
        "overflow".to_string(),
        Spec {
            page_size: 512,
            eytzinger: false,
            filter: None,
            tables: vec![TableSpec {
                name: "blobs".to_string(),
                encoding: "i64".to_string(),
                rows: spec_rows,
                indexes: vec![IndexSpec {
                    name: "by_key".to_string(),
                    unique: false,
                    encoding: "i64".to_string(),
                    key_from: "key".to_string(),
                }],
            }],
            queries: Queries {
                points,
                ranges,
                counts,
                ..Default::default()
            },
        },
    )
}

fn fixed_width_fixture(name: &str, encoding: &str, page_size: usize) -> (String, Spec) {
    let mut rng = Rng::new(0x51DE_4001);
    let mut spec_rows = Vec::new();
    let mut keys: Vec<Vec<u8>> = Vec::new();
    for i in 0..300u64 {
        let mut key = [0u8; 16];
        let stamp = 0x0190_0000_0000u64 + i * 1000 + rng.below(500);
        match encoding {
            // .NET Guid layout: the first three fields are little-endian.
            "uuidv7" => {
                key[0..4].copy_from_slice(&((stamp >> 16) as u32).to_le_bytes());
                key[4..6].copy_from_slice(&((stamp & 0xFFFF) as u16).to_le_bytes());
                key[6..8].copy_from_slice(&(0x7000u16 | (rng.below(0x0FFF) as u16)).to_le_bytes());
                for b in key.iter_mut().skip(8) {
                    *b = rng.below(256) as u8;
                }
            }
            // ULID: 16 bytes compared big-endian, timestamp first.
            _ => {
                key[0..6].copy_from_slice(&stamp.to_be_bytes()[2..]);
                for b in key.iter_mut().skip(6) {
                    *b = rng.below(256) as u8;
                }
            }
        }
        if keys.contains(&key.to_vec()) {
            continue;
        }
        keys.push(key.to_vec());
        spec_rows.push(row(key.to_vec(), format!("row-{i}").into_bytes()));
    }

    let mut sorted = keys.clone();
    let table = "ids";
    sorted.sort_by(|a, b| {
        let enc = encoding_for(encoding);
        enc.compare(a, b).expect("fixture keys are well formed")
    });
    let (ranges, counts) = bound_queries(table, &sorted);
    let points = sorted.iter().map(|k| point(table, k)).collect();

    (
        name.to_string(),
        Spec {
            page_size,
            eytzinger: false,
            filter: None,
            tables: vec![TableSpec {
                name: table.to_string(),
                encoding: encoding.to_string(),
                rows: spec_rows,
                indexes: Vec::new(),
            }],
            queries: Queries {
                points,
                ranges,
                counts,
                ..Default::default()
            },
        },
    )
}

/// Many rows sharing one index key, on an encoding whose exact digest makes upstream
/// drop the key bytes from the index pages entirely.
fn duplicate_index_fixture() -> (String, Spec) {
    let mut spec_rows = Vec::new();
    let keys: Vec<Vec<u8>> = (0..40i64).map(|i| i.to_le_bytes().to_vec()).collect();
    for (i, key) in keys.iter().enumerate() {
        spec_rows.push(row(key.clone(), format!("row-{i:03}").into_bytes()));
    }
    let constant = 0i64.to_le_bytes();
    let (ranges, counts) = bound_queries("t", &keys);

    (
        "duplicate-index".to_string(),
        Spec {
            page_size: 4096,
            eytzinger: false,
            filter: None,
            tables: vec![TableSpec {
                name: "t".to_string(),
                encoding: "i64".to_string(),
                rows: spec_rows,
                indexes: vec![IndexSpec {
                    name: "all_same".to_string(),
                    unique: false,
                    encoding: "i64".to_string(),
                    key_from: format!("const:{}", b64(&constant)),
                }],
            }],
            queries: Queries {
                points: keys.iter().map(|k| point("t", k)).collect(),
                ranges,
                counts,
                index_lookups: vec![IndexLookup {
                    table: "t".to_string(),
                    index: "all_same".to_string(),
                    key: b64(&constant),
                }],
                index_ranges: vec![
                    index_range("t", "all_same", &constant, &constant, false, false, "asc"),
                    index_range("t", "all_same", &constant, &constant, false, false, "desc"),
                ],
            },
        },
    )
}

fn fixtures() -> Vec<(String, Spec)> {
    let mut all = vec![
        i64_fixture("i64-small-omitted", 256, false, 300, 24, None),
        i64_fixture("i64-eytzinger", 256, true, 300, 24, None),
        i64_fixture("i64-default-page", 4096, false, 5000, 40, None),
        i64_fixture("i64-classic-meta", 40_000, false, 2000, 64, None),
        i64_fixture("i64-classic-meta-eytzinger", 40_000, true, 2000, 64, None),
        i64_fixture("i64-zstd", 4096, false, 2000, 64, Some("zstd".to_string())),
        ascii_fixture("ascii-small", 256, false, 400, None),
        ascii_fixture("ascii-eytzinger", 512, true, 400, None),
        ascii_fixture("ascii-default-page", 4096, false, 3000, None),
        ascii_fixture("ascii-zstd", 4096, false, 1500, Some("zstd".to_string())),
        overflow_fixture(),
        fixed_width_fixture("uuidv7", "uuidv7", 1024),
        fixed_width_fixture("ulid", "ulid", 1024),
        duplicate_index_fixture(),
    ];
    all.sort_by(|a, b| a.0.cmp(&b.0));
    all
}

fn describe_difference(label: &str, left: &Dump, right: &Dump) -> String {
    let mut lines = vec![format!("{label} differ")];
    for (i, (l, r)) in left.tables.iter().zip(&right.tables).enumerate() {
        if l.count != r.count {
            lines.push(format!("  table[{i}] count {} vs {}", l.count, r.count));
        }
        if l.scan.len() != r.scan.len() {
            lines.push(format!(
                "  table[{i}] scan length {} vs {}",
                l.scan.len(),
                r.scan.len()
            ));
        }
        for (n, (a, b)) in l.scan.iter().zip(&r.scan).enumerate() {
            if a != b {
                lines.push(format!("  table[{i}] scan row {n}: {a:?} vs {b:?}"));
                break;
            }
        }
        for (n, (a, b)) in l.scan_descending.iter().zip(&r.scan_descending).enumerate() {
            if a != b {
                lines.push(format!("  table[{i}] descending row {n}: {a:?} vs {b:?}"));
                break;
            }
        }
    }
    for (i, (a, b)) in left.points.iter().zip(&right.points).enumerate() {
        if a != b {
            lines.push(format!("  point[{i}]: {a:?} vs {b:?}"));
            break;
        }
    }
    for (i, (a, b)) in left.ranges.iter().zip(&right.ranges).enumerate() {
        if a != b {
            lines.push(format!("  range[{i}]: {} values vs {}", a.len(), b.len()));
            break;
        }
    }
    for (i, (a, b)) in left.counts.iter().zip(&right.counts).enumerate() {
        if a != b {
            lines.push(format!("  count[{i}]: {a} vs {b}"));
            break;
        }
    }
    for (i, (a, b)) in left
        .index_lookups
        .iter()
        .zip(&right.index_lookups)
        .enumerate()
    {
        if a != b {
            lines.push(format!(
                "  indexLookup[{i}]: {} values vs {}",
                a.len(),
                b.len()
            ));
            break;
        }
    }
    for (i, (a, b)) in left
        .index_ranges
        .iter()
        .zip(&right.index_ranges)
        .enumerate()
    {
        if a != b {
            lines.push(format!(
                "  indexRange[{i}]: {} values vs {}",
                a.len(),
                b.len()
            ));
            break;
        }
    }
    lines.join("\n")
}

fn read_dump(path: &Path) -> Dump {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("cannot read dump {}: {e}", path.display()));
    serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("cannot parse dump {}: {e}", path.display()))
}

/// Prepares the oracle, or returns `None` when the prerequisites are missing and the
/// test is allowed to skip.
fn oracle_or_skip() -> Option<Oracle> {
    let required = std::env::var("DRYDB_INTEROP").ok().as_deref() == Some("1");
    let root = repo_root();
    let upstream = root.join("tests/interop/upstream/src/DryDB/DryDB.csproj");

    if !upstream.exists() {
        let message = "upstream checkout missing; run tests/interop/fetch-upstream.sh";
        assert!(!required, "{message}");
        eprintln!("skipping interop: {message}");
        return None;
    }
    if !have_dotnet() {
        let message = "the .NET SDK is not on PATH";
        assert!(!required, "{message}");
        eprintln!("skipping interop: {message}");
        return None;
    }
    match Oracle::shared() {
        Ok(o) => Some(o),
        Err(e) => {
            assert!(!required, "{e}");
            eprintln!("skipping interop: {e}");
            None
        }
    }
}

fn work_dir(name: &str) -> PathBuf {
    let dir = repo_root().join("tests/interop/work").join(name);
    std::fs::create_dir_all(&dir).expect("cannot create the interop work directory");
    dir
}

#[test]
fn csharp_and_rust_agree() {
    let Some(oracle) = oracle_or_skip() else {
        return;
    };
    let root = repo_root();

    let work = root.join("tests/interop/work/fixtures");
    std::fs::create_dir_all(&work).expect("cannot create the interop work directory");

    let mut manifest = String::from(
        "# Interop fixture manifest\n\
         #\n\
         # upstream commit 6b175929491793948e63430c20c2d6f58300d97f, storage format 1.4\n\
         # name | page size | eytzinger | rows | sha256(C# build) | sha256(Rust build)\n",
    );

    for (name, spec) in fixtures() {
        let spec_path = work.join(format!("{name}.spec.json"));
        std::fs::write(
            &spec_path,
            serde_json::to_string(&spec).expect("serialise spec"),
        )
        .expect("write spec");

        let csharp_db = work.join(format!("{name}.csharp.drydb"));
        let rust_db = work.join(format!("{name}.rust.drydb"));

        oracle
            .build_db(&spec_path, &csharp_db)
            .unwrap_or_else(|e| panic!("[{name}] C# build failed: {e}"));
        build(&spec, &rust_db).unwrap_or_else(|e| panic!("[{name}] Rust build failed: {e}"));

        // C# file, both readers.
        let csharp_dump_path = work.join(format!("{name}.csharp-db.csharp-dump.json"));
        oracle
            .dump(&spec_path, &csharp_db, &csharp_dump_path)
            .unwrap_or_else(|e| panic!("[{name}] C# dump of the C# file failed: {e}"));
        let csharp_of_csharp = read_dump(&csharp_dump_path);

        let db = drydb::Database::open(&csharp_db)
            .unwrap_or_else(|e| panic!("[{name}] Rust cannot open the C# file: {e}"));
        let report = db.verify(Default::default()).expect("verify runs");
        assert!(
            report.is_ok(),
            "[{name}] verify of the C# file: {:?}",
            report.problems
        );
        let rust_of_csharp =
            dump(&spec, &db).unwrap_or_else(|e| panic!("[{name}] Rust dump failed: {e}"));
        drop(db);

        assert_eq!(
            rust_of_csharp,
            csharp_of_csharp,
            "[{name}] reading the C# file: {}",
            describe_difference(
                "Rust and C# dumps of the C# file",
                &rust_of_csharp,
                &csharp_of_csharp
            )
        );

        // Rust file, both readers.
        let csharp_dump_of_rust = work.join(format!("{name}.rust-db.csharp-dump.json"));
        oracle
            .dump(&spec_path, &rust_db, &csharp_dump_of_rust)
            .unwrap_or_else(|e| panic!("[{name}] C# cannot read the Rust file: {e}"));
        let csharp_of_rust = read_dump(&csharp_dump_of_rust);

        let db = drydb::Database::open(&rust_db)
            .unwrap_or_else(|e| panic!("[{name}] Rust cannot open its own file: {e}"));
        let report = db.verify(Default::default()).expect("verify runs");
        assert!(
            report.is_ok(),
            "[{name}] verify of the Rust file: {:?}",
            report.problems
        );
        let rust_of_rust =
            dump(&spec, &db).unwrap_or_else(|e| panic!("[{name}] Rust dump failed: {e}"));
        drop(db);

        assert_eq!(
            csharp_of_rust,
            rust_of_rust,
            "[{name}] reading the Rust file: {}",
            describe_difference(
                "C# and Rust dumps of the Rust file",
                &csharp_of_rust,
                &rust_of_rust
            )
        );

        // Same logical content whichever implementation wrote it.
        assert_eq!(
            csharp_of_csharp, rust_of_rust,
            "[{name}] the two builders produced different logical content"
        );

        let rows: usize = spec.tables.iter().map(|t| t.rows.len()).sum();
        manifest.push_str(&format!(
            "{name} | {} | {} | {rows} | {} | {}\n",
            spec.page_size,
            spec.eytzinger,
            file_sha256(&csharp_db),
            file_sha256(&rust_db),
        ));
    }

    std::fs::write(root.join("tests/interop/work/manifest.txt"), &manifest)
        .expect("cannot write the fixture manifest");
    eprintln!("{manifest}");
}

// ---------------------------------------------------------------------------
// Measured divergences.
//
// Each test below records a place where this crate deliberately does something other
// than upstream 1.4, with the upstream behaviour asserted rather than described, so
// that a change in either implementation shows up as a failure instead of a silent
// difference. `docs/compatibility.md` carries the same list in prose.
// ---------------------------------------------------------------------------

/// Spec with one table, one non-unique index and a caller-chosen index key width.
fn short_key_index_spec(index_key_len: usize) -> Spec {
    let rows: Vec<Row> = (0..30i64)
        .map(|i| {
            let mut value = b"AAAAAAAAAAAA".to_vec();
            value.extend_from_slice(format!("-{i:03}").as_bytes());
            row(i.to_le_bytes().to_vec(), value)
        })
        .collect();
    let index_key = b"AAAAAAAAAAAA"[..index_key_len].to_vec();
    Spec {
        page_size: 4096,
        eytzinger: false,
        filter: None,
        tables: vec![TableSpec {
            name: "t".to_string(),
            encoding: "i64".to_string(),
            rows,
            indexes: vec![IndexSpec {
                name: "by_head".to_string(),
                unique: false,
                encoding: "ascii".to_string(),
                key_from: format!("value_prefix:{index_key_len}"),
            }],
        }],
        queries: Queries {
            index_lookups: vec![IndexLookup {
                table: "t".to_string(),
                index: "by_head".to_string(),
                key: b64(&index_key),
            }],
            ..Default::default()
        },
    }
}

#[test]
fn divergence_short_non_unique_index_keys() {
    let Some(oracle) = oracle_or_skip() else {
        return;
    };
    let dir = work_dir("divergence");

    // Upstream computes a non-unique index page's digests over `source||record_id`,
    // but its reader computes the search digest over the source key alone. The two
    // only agree once the source key already fills the eight digest bytes, so a short
    // index key makes the reader's digest match only the row whose record id is zero.
    for (index_key_len, expected_csharp) in [(3usize, 1usize), (8, 30)] {
        let spec = short_key_index_spec(index_key_len);
        let spec_path = dir.join(format!("short-{index_key_len}.spec.json"));
        std::fs::write(&spec_path, serde_json::to_string(&spec).unwrap()).unwrap();

        let csharp_db = dir.join(format!("short-{index_key_len}.csharp.drydb"));
        oracle.build_db(&spec_path, &csharp_db).expect("C# build");
        let csharp_dump = dir.join(format!("short-{index_key_len}.csharp.json"));
        oracle
            .dump(&spec_path, &csharp_db, &csharp_dump)
            .expect("C# dump");
        let csharp = read_dump(&csharp_dump);
        assert_eq!(
            csharp.index_lookups[0].len(),
            expected_csharp,
            "upstream lookup on a {index_key_len} byte index key"
        );

        let db = drydb::Database::open(&csharp_db).expect("Rust opens the C# file");
        let rust = dump(&spec, &db).expect("Rust dump");
        assert_eq!(
            rust.index_lookups[0].len(),
            30,
            "this crate resolves every row for the key, whatever the key width"
        );
        drop(db);

        // A file written here carries digests the upstream reader agrees with, so the
        // same lookup returns every row on both sides.
        let rust_db = dir.join(format!("short-{index_key_len}.rust.drydb"));
        build(&spec, &rust_db).expect("Rust build");
        let csharp_of_rust = dir.join(format!("short-{index_key_len}.csharp-of-rust.json"));
        oracle
            .dump(&spec_path, &rust_db, &csharp_of_rust)
            .expect("C# dump of the Rust file");
        assert_eq!(
            read_dump(&csharp_of_rust).index_lookups[0].len(),
            30,
            "upstream reads every row out of a file written here"
        );
    }
}

#[test]
fn divergence_exclusive_bounds_on_a_non_unique_index() {
    let Some(oracle) = oracle_or_skip() else {
        return;
    };
    let dir = work_dir("divergence");

    // Two index keys, fifteen rows each.
    let rows: Vec<Row> = (0..30i64)
        .map(|i| {
            let head = if i < 15 { "AAAAAAAA" } else { "BBBBBBBB" };
            row(
                i.to_le_bytes().to_vec(),
                format!("{head}-{i:03}").into_bytes(),
            )
        })
        .collect();
    let spec = Spec {
        page_size: 4096,
        eytzinger: false,
        filter: None,
        tables: vec![TableSpec {
            name: "t".to_string(),
            encoding: "i64".to_string(),
            rows,
            indexes: vec![IndexSpec {
                name: "by_head".to_string(),
                unique: false,
                encoding: "ascii".to_string(),
                key_from: "value_prefix:8".to_string(),
            }],
        }],
        queries: Queries {
            index_ranges: vec![IndexRangeQuery {
                table: "t".to_string(),
                index: "by_head".to_string(),
                lower: Some(b64(b"AAAAAAAA")),
                upper: Some(b64(b"BBBBBBBB")),
                lower_exclusive: true,
                upper_exclusive: false,
                order: "asc".to_string(),
            }],
            ..Default::default()
        },
    };

    let spec_path = dir.join("exclusive.spec.json");
    std::fs::write(&spec_path, serde_json::to_string(&spec).unwrap()).unwrap();
    let csharp_db = dir.join("exclusive.csharp.drydb");
    oracle.build_db(&spec_path, &csharp_db).expect("C# build");
    let csharp_dump = dir.join("exclusive.csharp.json");
    oracle
        .dump(&spec_path, &csharp_db, &csharp_dump)
        .expect("C# dump");

    // Upstream turns the exclusive bound into `(key, record_id 0)`, so it only drops
    // the first of the fifteen rows filed under `AAAAAAAA`.
    assert_eq!(read_dump(&csharp_dump).index_ranges[0].len(), 29);

    // Here an exclusive bound on an index key clears every row filed under it.
    let db = drydb::Database::open(&csharp_db).expect("Rust opens the C# file");
    let rust = dump(&spec, &db).expect("Rust dump");
    assert_eq!(rust.index_ranges[0].len(), 15);
}

#[test]
fn divergence_empty_table_root() {
    let Some(oracle) = oracle_or_skip() else {
        return;
    };
    let dir = work_dir("divergence");

    // Upstream writes no page at all for a table with no rows and records the root
    // ordinal as -1. Two things follow, both asserted here: a database whose tables are
    // all empty has no pages and the upstream builder itself fails on it, and a
    // half-empty database builds but its own reader indexes the page directory with -1.
    let empty_only = Spec {
        page_size: 4096,
        eytzinger: false,
        filter: None,
        tables: vec![TableSpec {
            name: "empty".to_string(),
            encoding: "i64".to_string(),
            rows: Vec::new(),
            indexes: Vec::new(),
        }],
        queries: Queries::default(),
    };
    let spec_path = dir.join("empty-only.spec.json");
    std::fs::write(&spec_path, serde_json::to_string(&empty_only).unwrap()).unwrap();
    let csharp_db = dir.join("empty-only.csharp.drydb");
    assert!(
        oracle.build_db(&spec_path, &csharp_db).is_err(),
        "upstream is expected to fail building a database with no pages"
    );

    // This crate writes a real, empty leaf page, so the file exists and both readers
    // agree that the table is empty.
    let rust_db = dir.join("empty-only.rust.drydb");
    build(&empty_only, &rust_db).expect("Rust build");
    let csharp_of_rust = dir.join("empty-only.csharp-of-rust.json");
    oracle
        .dump(&spec_path, &rust_db, &csharp_of_rust)
        .expect("upstream reads an empty table written here");
    assert_eq!(read_dump(&csharp_of_rust).tables[0].count, 0);
    let db = drydb::Database::open(&rust_db).expect("Rust opens its own file");
    assert_eq!(db.table("empty").unwrap().count().unwrap(), 0);
    drop(db);

    // Mixed database: upstream builds it, then cannot read the empty table back.
    let mixed = Spec {
        page_size: 4096,
        eytzinger: false,
        filter: None,
        tables: vec![
            TableSpec {
                name: "empty".to_string(),
                encoding: "i64".to_string(),
                rows: Vec::new(),
                indexes: Vec::new(),
            },
            TableSpec {
                name: "full".to_string(),
                encoding: "i64".to_string(),
                rows: vec![row(1i64.to_le_bytes().to_vec(), b"v".to_vec())],
                indexes: Vec::new(),
            },
        ],
        queries: Queries::default(),
    };
    let mixed_path = dir.join("mixed.spec.json");
    std::fs::write(&mixed_path, serde_json::to_string(&mixed).unwrap()).unwrap();
    let mixed_db = dir.join("mixed.csharp.drydb");
    oracle
        .build_db(&mixed_path, &mixed_db)
        .expect("C# build of the mixed database");
    let mixed_dump = dir.join("mixed.csharp.json");
    assert!(
        oracle.dump(&mixed_path, &mixed_db, &mixed_dump).is_err(),
        "upstream is expected to fail reading its own empty table"
    );

    let db = drydb::Database::open(&mixed_db).expect("Rust opens the C# file");
    assert_eq!(db.table("empty").unwrap().count().unwrap(), 0);
    assert_eq!(db.table("full").unwrap().count().unwrap(), 1);
}

#[test]
fn divergence_inline_value_length_overflow() {
    let Some(oracle) = oracle_or_skip() else {
        return;
    };
    let dir = work_dir("divergence");

    // A page large enough to hold a 100 kB value inline. Upstream stores the inline
    // length in a u16, so it writes 100_000 as 34_464 and the value is truncated on
    // read; here anything at or above the 0xFFFF sentinel goes to a blob page instead.
    let value: Vec<u8> = (0..100_000).map(|n| (n % 251) as u8).collect();
    let spec = Spec {
        page_size: 200_000,
        eytzinger: false,
        filter: None,
        tables: vec![TableSpec {
            name: "t".to_string(),
            encoding: "i64".to_string(),
            rows: vec![row(1i64.to_le_bytes().to_vec(), value.clone())],
            indexes: Vec::new(),
        }],
        queries: Queries {
            points: vec![point("t", &1i64.to_le_bytes())],
            ..Default::default()
        },
    };
    let spec_path = dir.join("wide.spec.json");
    std::fs::write(&spec_path, serde_json::to_string(&spec).unwrap()).unwrap();

    let csharp_db = dir.join("wide.csharp.drydb");
    oracle.build_db(&spec_path, &csharp_db).expect("C# build");
    let csharp_dump = dir.join("wide.csharp.json");
    oracle
        .dump(&spec_path, &csharp_db, &csharp_dump)
        .expect("C# dump");
    let truncated = unb64(read_dump(&csharp_dump).points[0].value.as_ref().unwrap());
    assert_eq!(
        truncated.len(),
        100_000 - 65_536,
        "upstream truncates the stored length"
    );

    // Both readers see the same truncated value, because that is what the file says.
    let db = drydb::Database::open(&csharp_db).expect("Rust opens the C# file");
    let rust = dump(&spec, &db).expect("Rust dump");
    assert_eq!(
        unb64(rust.points[0].value.as_ref().unwrap()).len(),
        truncated.len()
    );
    drop(db);

    // A file written here keeps the value whole, and upstream reads it back whole.
    let rust_db = dir.join("wide.rust.drydb");
    build(&spec, &rust_db).expect("Rust build");
    let csharp_of_rust = dir.join("wide.csharp-of-rust.json");
    oracle
        .dump(&spec_path, &rust_db, &csharp_of_rust)
        .expect("C# dump of the Rust file");
    assert_eq!(
        unb64(read_dump(&csharp_of_rust).points[0].value.as_ref().unwrap()),
        value,
        "upstream reads the whole value out of a file written here"
    );
}

#[test]
fn divergence_a_page_too_small_to_hold_two_separators() {
    let Some(oracle) = oracle_or_skip() else {
        return;
    };
    let dir = work_dir("divergence");

    // A 128 byte page with Eytzinger digests holds one 29 byte separator but not two.
    // A level whose pages fit one separator each promotes as many entries as it
    // received, so the tree gains a level per rotation and the entry count never falls.
    let spec = Spec {
        page_size: 128,
        eytzinger: true,
        filter: None,
        tables: vec![TableSpec {
            name: "t".to_string(),
            encoding: "ascii".to_string(),
            rows: vec![
                row(vec![b'a'; 29], b"xyz".to_vec()),
                row(vec![b'b'; 33], b"uv".to_vec()),
            ],
            indexes: Vec::new(),
        }],
        queries: Queries::default(),
    };
    let spec_path = dir.join("narrow-page.spec.json");
    std::fs::write(&spec_path, serde_json::to_string(&spec).unwrap()).unwrap();
    let csharp_db = dir.join("narrow-page.csharp.drydb");

    // Upstream does not finish. Measured here: still running after 45 seconds, with a
    // 527 MB output file. The bound below is short enough to keep the test cheap.
    let finished = oracle
        .run_bounded(
            &[Path::new("build"), &spec_path, &csharp_db],
            Duration::from_secs(10),
        )
        .expect("the oracle can be started");
    assert!(
        !finished,
        "upstream is expected not to converge on this input"
    );
    let _ = std::fs::remove_file(&csharp_db);

    // This crate refuses the key instead, naming the reason.
    let err = build(&spec, &dir.join("narrow-page.rust.drydb")).unwrap_err();
    assert_eq!(err.kind(), drydb::ErrorKind::ValueTooLarge);
    assert!(err.to_string().contains("two separators"), "{err}");
}

#[test]
fn divergence_duplicate_index_keys_spanning_leaves() {
    let Some(oracle) = oracle_or_skip() else {
        return;
    };
    let dir = work_dir("divergence");

    // Forty rows under one index key, on a page small enough that they need several
    // leaves. The index key encoding has an exact digest, so upstream writes the index
    // pages without key bytes and every separator on the internal page looks identical.
    let rows: Vec<Row> = (0..40i64)
        .map(|i| {
            row(
                i.to_le_bytes().to_vec(),
                format!("value-{i:04}").into_bytes(),
            )
        })
        .collect();
    let constant = 0i64.to_le_bytes();
    let spec = Spec {
        page_size: 256,
        eytzinger: false,
        filter: None,
        tables: vec![TableSpec {
            name: "t".to_string(),
            encoding: "i64".to_string(),
            rows,
            indexes: vec![IndexSpec {
                name: "all_same".to_string(),
                unique: false,
                encoding: "i64".to_string(),
                key_from: format!("const:{}", b64(&constant)),
            }],
        }],
        queries: Queries {
            index_lookups: vec![IndexLookup {
                table: "t".to_string(),
                index: "all_same".to_string(),
                key: b64(&constant),
            }],
            ..Default::default()
        },
    };

    let spec_path = dir.join("spanning.spec.json");
    std::fs::write(&spec_path, serde_json::to_string(&spec).unwrap()).unwrap();
    let csharp_db = dir.join("spanning.csharp.drydb");
    oracle.build_db(&spec_path, &csharp_db).expect("C# build");
    let dump_path = dir.join("spanning.csharp.json");
    oracle
        .dump(&spec_path, &csharp_db, &dump_path)
        .expect("C# dump");
    let csharp_rows = read_dump(&dump_path).index_lookups[0].len();

    // Upstream's descent walks past every child whose separator compares equal and takes
    // the last one, so the earlier leaves are never reached.
    assert!(
        csharp_rows < 40,
        "upstream is expected to miss rows here, but returned {csharp_rows}"
    );

    // This crate descends into the first child of the run and walks right, so it reaches
    // all of them.
    let db = drydb::Database::open(&csharp_db).expect("Rust opens the C# file");
    let rust = dump(&spec, &db).expect("Rust dump");
    assert_eq!(
        rust.index_lookups[0].len(),
        40,
        "every row filed under the key should be reachable"
    );
    let table = db.table("t").unwrap();
    let index = table.index("all_same").unwrap();
    assert_eq!(
        index
            .count_range(
                std::ops::Bound::Included(&constant[..]),
                std::ops::Bound::Included(&constant[..])
            )
            .unwrap(),
        40
    );
    drop(db);

    // A file written here stores the key bytes on non-unique index pages, so upstream
    // reads every row out of it too.
    let rust_db = dir.join("spanning.rust.drydb");
    build(&spec, &rust_db).expect("Rust build");
    let rust_dump = dir.join("spanning.csharp-of-rust.json");
    oracle
        .dump(&spec_path, &rust_db, &rust_dump)
        .expect("C# dump of the Rust file");
    assert_eq!(read_dump(&rust_dump).index_lookups[0].len(), 40);
}

/// Two index keys, with the boundary between them falling inside a leaf.
///
/// The separator on the internal page for the second child then carries the digest of
/// the later key, while the rows under that key start on the child before it. Upstream
/// descends into the child whose separator compares equal and reports only the tail;
/// this crate descends into the last child that starts strictly below the key and walks
/// right, which reaches all of them.
#[test]
fn divergence_index_key_starting_inside_a_leaf() {
    let Some(oracle) = oracle_or_skip() else {
        return;
    };
    let dir = work_dir("divergence");

    // The index key is the first eight bytes of the value: zero for the first five rows,
    // one for the other thirty-five.
    let rows: Vec<Row> = (0..40i64)
        .map(|i| {
            let group: i64 = if i < 5 { 0 } else { 1 };
            let mut value = group.to_le_bytes().to_vec();
            value.extend_from_slice(format!("-{i:04}").as_bytes());
            row(i.to_le_bytes().to_vec(), value)
        })
        .collect();
    let wanted = 1i64.to_le_bytes();
    let spec = Spec {
        page_size: 256,
        eytzinger: false,
        filter: None,
        tables: vec![TableSpec {
            name: "t".to_string(),
            encoding: "i64".to_string(),
            rows,
            indexes: vec![IndexSpec {
                name: "grp".to_string(),
                unique: false,
                encoding: "i64".to_string(),
                key_from: "value_prefix:8".to_string(),
            }],
        }],
        queries: Queries {
            index_lookups: vec![IndexLookup {
                table: "t".to_string(),
                index: "grp".to_string(),
                key: b64(&wanted),
            }],
            ..Default::default()
        },
    };

    let spec_path = dir.join("midleaf.spec.json");
    std::fs::write(&spec_path, serde_json::to_string(&spec).unwrap()).unwrap();
    let csharp_db = dir.join("midleaf.csharp.drydb");
    oracle.build_db(&spec_path, &csharp_db).expect("C# build");
    let dump_path = dir.join("midleaf.csharp.json");
    oracle
        .dump(&spec_path, &csharp_db, &dump_path)
        .expect("C# dump");
    let csharp_rows = read_dump(&dump_path).index_lookups[0].len();
    assert_eq!(
        csharp_rows, 4,
        "measured against the pinned upstream commit: it reaches only the last child \
         whose separator compares equal"
    );

    let db = drydb::Database::open(&csharp_db).expect("Rust opens the C# file");
    let rust = dump(&spec, &db).expect("Rust dump");
    assert_eq!(
        rust.index_lookups[0].len(),
        35,
        "every row under the index key should be reachable"
    );
    let table = db.table("t").unwrap();
    let index = table.index("grp").unwrap();
    assert_eq!(
        index
            .count_range(
                std::ops::Bound::Included(&wanted[..]),
                std::ops::Bound::Included(&wanted[..])
            )
            .unwrap(),
        35
    );
    // The other key is unaffected.
    let zero = 0i64.to_le_bytes();
    assert_eq!(
        index
            .count_range(
                std::ops::Bound::Included(&zero[..]),
                std::ops::Bound::Included(&zero[..])
            )
            .unwrap(),
        5
    );
    // And a key that was never indexed is still absent.
    let absent = 7i64.to_le_bytes();
    let mut cursor = index.lookup(&absent).unwrap();
    assert!(!cursor.advance().unwrap());
}

/// The C# builder digests the whole composite key of a non-unique index, record id
/// included, and the record id is little-endian, so the digest falls when the low byte
/// carries into the next one. `docs/compatibility.md` D1 says such a file is still read
/// here, which means a page check must not require these digests to ascend.
#[test]
fn a_non_unique_index_whose_digests_fall_at_a_carry_is_readable() {
    let Some(oracle) = oracle_or_skip() else {
        return;
    };
    let dir = work_dir("divergence");

    // Three hundred rows, so the record id crosses 255 into 256.
    let rows: Vec<Row> = (0..300i64)
        .map(|i| {
            row(
                i.to_le_bytes().to_vec(),
                format!("value-{i:04}").into_bytes(),
            )
        })
        .collect();
    let constant = b"AAA";
    let spec = Spec {
        page_size: 16384,
        eytzinger: false,
        filter: None,
        tables: vec![TableSpec {
            name: "t".to_string(),
            encoding: "i64".to_string(),
            rows,
            indexes: vec![IndexSpec {
                name: "all_same".to_string(),
                unique: false,
                encoding: "ascii".to_string(),
                key_from: format!("const:{}", b64(constant)),
            }],
        }],
        queries: Queries {
            index_lookups: vec![IndexLookup {
                table: "t".to_string(),
                index: "all_same".to_string(),
                key: b64(constant),
            }],
            ..Default::default()
        },
    };

    let spec_path = dir.join("carry.spec.json");
    std::fs::write(&spec_path, serde_json::to_string(&spec).unwrap()).unwrap();
    let csharp_db = dir.join("carry.csharp.drydb");
    oracle.build_db(&spec_path, &csharp_db).expect("C# build");

    let db = drydb::Database::open(&csharp_db).expect("Rust opens the C# file");
    let rust = dump(&spec, &db).expect("Rust dump");
    assert_eq!(
        rust.index_lookups[0].len(),
        300,
        "every row filed under the key should be reachable"
    );
    let table = db.table("t").unwrap();
    assert_eq!(
        table
            .index("all_same")
            .unwrap()
            .count_range(
                std::ops::Bound::Included(&constant[..]),
                std::ops::Bound::Included(&constant[..])
            )
            .unwrap(),
        300
    );
    // The whole file passes verification, not only this query.
    let report = db.verify(Default::default()).expect("verify runs");
    assert!(report.is_ok(), "{:?}", report.problems);
}
