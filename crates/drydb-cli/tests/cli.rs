//! Drives the built binary the way a user would.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn work_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("drydb-cli-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("work dir");
    dir
}

fn drydb(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_drydb"))
        .args(args)
        .output()
        .expect("run drydb")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn build_sample(dir: &Path, extra: &[&str]) -> PathBuf {
    let input = dir.join("rows.tsv");
    let mut text = String::new();
    for i in 0..60 {
        text.push_str(&format!("key{i:03}\tvalue-{i}\n"));
    }
    std::fs::write(&input, text).expect("write input");

    let db = dir.join("db.drydb");
    let mut args: Vec<String> = vec![
        "build".into(),
        db.display().to_string(),
        "--table".into(),
        "words".into(),
        "--encoding".into(),
        "ascii".into(),
        "--input".into(),
        input.display().to_string(),
        "--page-size".into(),
        "256".into(),
    ];
    args.extend(extra.iter().map(|s| s.to_string()));
    let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let output = drydb(&refs);
    assert!(
        output.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    db
}

#[test]
fn build_inspect_verify_and_query() {
    let dir = work_dir("basic");
    let db = build_sample(&dir, &[]);
    let path = db.display().to_string();

    let output = drydb(&["inspect", &path]);
    assert!(output.status.success());
    let text = stdout(&output);
    assert!(text.contains("DryDB 1.4"), "{text}");
    assert!(text.contains("table `words`"), "{text}");
    assert!(text.contains("ascii"), "{text}");

    let output = drydb(&["verify", &path]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout(&output).contains("result:          ok"));

    let output = drydb(&["get", &path, "words", "key007"]);
    assert!(output.status.success());
    assert_eq!(stdout(&output).trim(), "value-7");

    let output = drydb(&["get", &path, "words", "missing"]);
    assert!(
        !output.status.success(),
        "a missing key should exit non-zero"
    );

    let output = drydb(&[
        "range", &path, "words", "--from", "key005", "--to", "key007",
    ]);
    assert!(output.status.success());
    assert_eq!(stdout(&output).lines().count(), 3);

    let output = drydb(&[
        "range",
        &path,
        "words",
        "--from",
        "key005",
        "--to",
        "key007",
        "--to-exclusive",
    ]);
    assert_eq!(stdout(&output).lines().count(), 2);

    let output = drydb(&["range", &path, "words", "--desc", "--limit", "4"]);
    let text = stdout(&output);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 4);
    assert!(lines[0].starts_with("key059"), "{lines:?}");

    let output = drydb(&["prefix", &path, "words", "key01", "--limit", "3"]);
    assert_eq!(stdout(&output).lines().count(), 3);

    let output = drydb(&["count", &path, "words"]);
    assert_eq!(stdout(&output).trim(), "60");

    let output = drydb(&[
        "count", &path, "words", "--from", "key010", "--to", "key019",
    ]);
    assert_eq!(stdout(&output).trim(), "10");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn key_and_value_formats() {
    let dir = work_dir("formats");
    let db = build_sample(&dir, &[]);
    let path = db.display().to_string();

    let output = drydb(&["get", &path, "words", "6b6579303030", "--key-format", "hex"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output = drydb(&[
        "range",
        &path,
        "words",
        "--from",
        "key000",
        "--to",
        "key000",
        "--value-format",
        "hex",
    ]);
    assert!(
        stdout(&output).contains("76616c75652d30"),
        "{}",
        stdout(&output)
    );

    let output = drydb(&[
        "range",
        &path,
        "words",
        "--from",
        "key000",
        "--to",
        "key000",
        "--value-format",
        "len",
    ]);
    assert!(stdout(&output).trim().ends_with('7'), "{}", stdout(&output));

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn i64_keys_are_written_as_decimals() {
    let dir = work_dir("i64");
    let input = dir.join("rows.tsv");
    let mut text = String::new();
    for i in [-5i64, 0, 7, i64::MAX, i64::MIN] {
        text.push_str(&format!("{i}\tvalue{i}\n"));
    }
    std::fs::write(&input, text).expect("write input");

    let db = dir.join("db.drydb");
    let output = drydb(&[
        "build",
        &db.display().to_string(),
        "--table",
        "items",
        "--encoding",
        "i64",
        "--input",
        &input.display().to_string(),
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let path = db.display().to_string();
    let output = drydb(&["get", &path, "items", "-5"]);
    assert_eq!(stdout(&output).trim(), "value-5");

    let output = drydb(&["range", &path, "items"]);
    let text = stdout(&output);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 5);
    assert!(lines[0].starts_with(&i64::MIN.to_string()), "{lines:?}");
    assert!(lines[4].starts_with(&i64::MAX.to_string()), "{lines:?}");

    // A byte prefix does not describe an i64 range, so it is refused rather than
    // answered wrongly.
    let output = drydb(&["prefix", &path, "items", "0"]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("does not order keys by their bytes"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn compressed_databases_round_trip() {
    let dir = work_dir("zstd");
    let db = build_sample(&dir, &["--zstd"]);
    let path = db.display().to_string();

    let output = drydb(&["inspect", &path]);
    assert!(
        stdout(&output).contains("DryDB.ZstdCompression"),
        "{}",
        stdout(&output)
    );

    let output = drydb(&["verify", &path]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output = drydb(&["get", &path, "words", "key042"]);
    assert_eq!(stdout(&output).trim(), "value-42");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_damaged_file_is_reported() {
    let dir = work_dir("damaged");
    let db = build_sample(&dir, &[]);
    let mut bytes = std::fs::read(&db).unwrap();
    // Corrupt the middle of the page area.
    let target = bytes.len() / 2;
    bytes[target] ^= 0xFF;
    bytes[target + 1] ^= 0xFF;
    std::fs::write(&db, bytes).unwrap();

    let output = drydb(&["verify", &db.display().to_string()]);
    // Either the file no longer opens, or verify names the problem; both exit non-zero.
    assert!(!output.status.success());

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn unknown_encodings_and_tables_are_named() {
    let dir = work_dir("errors");
    let db = build_sample(&dir, &[]);
    let path = db.display().to_string();

    let output = drydb(&["get", &path, "nosuch", "key000"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("nosuch"));

    let input = dir.join("rows.tsv");
    let output = drydb(&[
        "build",
        &dir.join("other.drydb").display().to_string(),
        "--table",
        "t",
        "--encoding",
        "nope",
        "--input",
        &input.display().to_string(),
    ]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("nope"));

    std::fs::remove_dir_all(&dir).ok();
}

/// A pass that stopped before it could examine anything reports nothing found, which is
/// not the same as finding nothing. The exit code used to say the database was sound.
#[test]
fn a_verification_that_could_not_run_is_not_reported_as_success() {
    let dir = work_dir("incomplete-verify");
    let db = build_sample(&dir, &[]);
    let path = db.display().to_string();

    // A limit of zero leaves no room to record anything, so the pass stops before the
    // first page: no problems, nothing checked, and no grounds to call the file sound.
    let output = drydb(&["verify", &path, "--max-problems", "0"]);
    let text = stdout(&output);
    assert!(
        !output.status.success(),
        "an unfinished verification must not exit zero: {text}"
    );
    assert!(text.contains("incomplete"), "{text}");
    assert!(!text.contains("result:          ok"), "{text}");
    assert!(text.contains("pages checked:   0"), "{text}");

    // Turning both passes off asks for no checking at all, which is refused rather than
    // answered with a clean bill of health.
    let output = drydb(&["verify", &path, "--no-pages", "--no-trees"]);
    assert!(!output.status.success());
    assert!(!stdout(&output).contains("result:          ok"));

    // The same file, actually checked, is sound.
    let output = drydb(&["verify", &path]);
    assert!(output.status.success());
    assert!(stdout(&output).contains("result:          ok"));

    // Either pass on its own still counts as a verification.
    for flag in ["--no-pages", "--no-trees"] {
        let output = drydb(&["verify", &path, flag]);
        assert!(output.status.success());
        assert!(stdout(&output).contains("result:          ok"));
    }

    std::fs::remove_dir_all(&dir).ok();
}

/// Scanning by prefix used to run the key width check a whole key gets, which refuses
/// every prefix of a fixed width encoding: the one thing the command is for.
#[test]
fn a_short_prefix_of_a_fixed_width_key_is_accepted() {
    let dir = work_dir("short-prefix");
    let input = dir.join("rows.tsv");
    let mut text = String::new();
    for (i, lead) in ["01", "01", "01", "02", "02"].iter().enumerate() {
        text.push_str(&format!("{lead}{:030x}\trow-{i}\n", i));
    }
    std::fs::write(&input, text).expect("write input");

    let db = dir.join("ids.drydb");
    let path = db.display().to_string();
    let output = drydb(&[
        "build",
        &path,
        "--table",
        "ids",
        "--encoding",
        "ulid",
        "--input",
        &input.display().to_string(),
        "--page-size",
        "256",
    ]);
    assert!(
        output.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output = drydb(&["prefix", &path, "ids", "01", "--key-format", "hex"]);
    let listed = stdout(&output);
    assert!(
        output.status.success(),
        "a one byte prefix has to be accepted: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(listed.lines().count(), 3, "{listed}");
    for line in listed.lines() {
        assert!(line.starts_with("01"), "{line}");
    }

    // An empty prefix still selects everything.
    let output = drydb(&["prefix", &path, "ids", "", "--key-format", "hex"]);
    assert!(output.status.success());
    assert_eq!(stdout(&output).lines().count(), 5);

    // A whole key is still checked when it is used as a key.
    let output = drydb(&["get", &path, "ids", "01", "--key-format", "hex"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("16"));

    std::fs::remove_dir_all(&dir).ok();
}

/// `--limit 0` asks for no rows. The limit was checked after a row had been printed, so
/// it produced one.
#[test]
fn a_limit_of_zero_prints_nothing() {
    let dir = work_dir("limit-zero");
    let input = dir.join("rows.tsv");
    std::fs::write(&input, "a\tfirst\nb\tsecond\n").expect("write input");

    let db = dir.join("two.drydb");
    let path = db.display().to_string();
    let output = drydb(&[
        "build",
        &path,
        "--table",
        "t",
        "--encoding",
        "ascii",
        "--input",
        &input.display().to_string(),
        "--page-size",
        "256",
    ]);
    assert!(
        output.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    for args in [
        vec!["range", &path, "t", "--limit", "0"],
        vec!["prefix", &path, "t", "a", "--limit", "0"],
    ] {
        let output = drydb(&args);
        assert!(output.status.success(), "{args:?}");
        assert_eq!(stdout(&output), "", "{args:?}");
    }

    // One still means one, and no limit still means everything.
    let output = drydb(&["range", &path, "t", "--limit", "1"]);
    assert_eq!(stdout(&output).lines().count(), 1);
    let output = drydb(&["range", &path, "t"]);
    assert_eq!(stdout(&output).lines().count(), 2);

    std::fs::remove_dir_all(&dir).ok();
}

/// A key printed by one command is a key someone will paste into the next. `uuidv7`
/// printed the standard hyphenated form and accepted only the stored bytes as hex, so
/// the key from a listing could not be looked up.
#[test]
fn a_key_as_printed_can_be_looked_up() {
    let dir = work_dir("key-round-trip");
    let input = dir.join("rows.tsv");
    std::fs::write(&input, "5d0a890196ac4b77bcceb302099a8057\t6869\n").expect("write input");

    let db = dir.join("ids.drydb");
    let path = db.display().to_string();
    let output = drydb(&[
        "build",
        &path,
        "--table",
        "t",
        "--encoding",
        "uuidv7",
        "--input",
        &input.display().to_string(),
        "--input-format",
        "hex",
        "--page-size",
        "256",
    ]);
    assert!(
        output.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let listed = stdout(&drydb(&["range", &path, "t"]));
    let printed = listed
        .lines()
        .next()
        .and_then(|line| line.split('\t').next())
        .expect("a row")
        .to_string();
    assert!(printed.contains('-'), "printed as a uuid: {printed}");

    let output = drydb(&["get", &path, "t", &printed]);
    assert!(
        output.status.success(),
        "the key as printed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(stdout(&output).trim(), "hi");

    // The bytes as the file holds them still work, which is what a dump would have.
    let output = drydb(&["get", &path, "t", "5d0a890196ac4b77bcceb302099a8057"]);
    assert!(output.status.success());
    assert_eq!(stdout(&output).trim(), "hi");

    // And so does an i64 key, which prints as a decimal.
    let input = dir.join("nums.tsv");
    std::fs::write(&input, "-7\tseven\n").expect("write input");
    let numbers = dir.join("nums.drydb");
    let numbers = numbers.display().to_string();
    let output = drydb(&[
        "build",
        &numbers,
        "--table",
        "t",
        "--encoding",
        "i64",
        "--input",
        &input.display().to_string(),
        "--page-size",
        "256",
    ]);
    assert!(output.status.success());
    let listed = stdout(&drydb(&["range", &numbers, "t"]));
    assert!(listed.starts_with("-7\t"), "{listed}");
    let output = drydb(&["get", &numbers, "t", "-7"]);
    assert_eq!(stdout(&output).trim(), "seven");

    std::fs::remove_dir_all(&dir).ok();
}

/// Printing a key as text used to go through `from_utf8_lossy`, which turns every byte
/// it cannot read into the same character. Two keys then printed the same, and looking
/// one up by what was printed answered with the other row.
#[test]
fn a_key_printed_as_text_reads_back_as_itself() {
    let dir = work_dir("text-keys");
    let input = dir.join("rows.tsv");
    // A key that is not UTF-8, the replacement character it used to print as, and a key
    // that is ordinary text.
    std::fs::write(&input, "ff\t61\nefbfbd\t62\ne38182\t63\n").expect("write input");
    let db = dir.join("db.drydb");
    let path = db.display().to_string();

    let output = drydb(&[
        "build",
        &path,
        "--table",
        "t",
        "--encoding",
        "ascii",
        "--input",
        &input.display().to_string(),
        "--input-format",
        "hex",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output = drydb(&[
        "range",
        &path,
        "t",
        "--key-format",
        "text",
        "--value-format",
        "text",
    ]);
    assert!(output.status.success());
    let listing = stdout(&output);
    let printed: Vec<(&str, &str)> = listing
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| line.split_once('\t').expect("key and value"))
        .collect();
    assert_eq!(printed.len(), 3, "{listing}");

    let mut keys: Vec<&str> = printed.iter().map(|(key, _)| *key).collect();
    keys.sort_unstable();
    keys.dedup();
    assert_eq!(keys.len(), 3, "each key prints as itself: {listing}");

    // Every printed key looks its own row up again.
    for (key, value) in printed {
        let output = drydb(&["get", &path, "t", key, "--key-format", "text"]);
        assert!(
            output.status.success(),
            "get {key}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(stdout(&output).trim_end(), value, "get {key}");
    }
}

/// What `build` does with the key column is what `--input-format` says it does: for
/// `ascii` it reads the escaped text these tools print, so `\xNN` in the input is the
/// byte it names rather than four characters. The help used to call it raw text.
#[test]
fn a_built_key_is_read_the_way_the_input_format_says() {
    let dir = work_dir("input-escapes");
    let input = dir.join("rows.tsv");
    std::fs::write(&input, "key\\x41\tvalue\n").expect("write input");
    let db = dir.join("db.drydb");
    let path = db.display().to_string();

    let output = drydb(&[
        "build",
        &path,
        "--table",
        "t",
        "--encoding",
        "ascii",
        "--input",
        &input.display().to_string(),
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    // The escape named a byte, so the stored key is `keyA`.
    let output = drydb(&["get", &path, "t", "6b657941", "--key-format", "hex"]);
    assert!(output.status.success());
    assert_eq!(stdout(&output).trim_end(), "value");

    // And the listing prints it as `keyA`, which looks itself up again.
    let output = drydb(&["range", &path, "t", "--value-format", "text"]);
    assert!(output.status.success());
    let listing = stdout(&output);
    assert!(listing.starts_with("keyA\tvalue"), "{listing}");
}
