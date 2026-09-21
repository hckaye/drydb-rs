//! Command line tools for DryDB 1.4 files.
//!
//! Everything here runs through the same bounded page store the library uses, so
//! inspecting or verifying a database far larger than memory costs the budget you give
//! it, not the size of the file.

use std::io::{BufRead, Write};
use std::ops::Bound;
use std::path::PathBuf;
use std::sync::Arc;

use clap::{Args, Parser, Subcommand, ValueEnum};
use drydb::{
    AsciiEncoding, Database, DatabaseBuilder, Error, ErrorKind, Int64Encoding, KeyEncoding,
    OpenOptions, Order, Result, UlidEncoding, Uuidv7Encoding, VerifyOptions, ZstdFilter,
};

#[derive(Parser, Debug)]
#[command(
    name = "drydb",
    about = "Inspect, verify, query and build DryDB 1.4 database files",
    version
)]
struct Cli {
    /// Managed memory budget in bytes. Queries fail rather than exceed it.
    #[arg(long, global = true, default_value_t = 64 * 1024 * 1024)]
    budget: u64,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Print the header, filters, tables and indexes.
    Inspect {
        /// Database file.
        path: PathBuf,
    },
    /// Walk every page and every tree, reporting structural problems.
    Verify {
        /// Database file.
        path: PathBuf,
        /// Skip the page sweep.
        #[arg(long)]
        no_pages: bool,
        /// Skip the tree walk.
        #[arg(long)]
        no_trees: bool,
        /// Stop after this many problems.
        #[arg(long, default_value_t = 100)]
        max_problems: usize,
    },
    /// Look one key up.
    #[command(allow_negative_numbers = true)]
    Get {
        /// Database file.
        path: PathBuf,
        /// Table name.
        table: String,
        /// Key, in the format given by --key-format.
        key: String,
        /// Read through this secondary index instead of the primary key.
        #[arg(long)]
        index: Option<String>,
        #[command(flatten)]
        format: FormatArgs,
    },
    /// Scan a key range.
    #[command(allow_negative_numbers = true)]
    Range {
        /// Database file.
        path: PathBuf,
        /// Table name.
        table: String,
        /// Lower bound. Omit for unbounded.
        #[arg(long)]
        from: Option<String>,
        /// Upper bound. Omit for unbounded.
        #[arg(long)]
        to: Option<String>,
        /// Exclude the lower bound.
        #[arg(long)]
        from_exclusive: bool,
        /// Exclude the upper bound.
        #[arg(long)]
        to_exclusive: bool,
        /// Largest key first.
        #[arg(long)]
        desc: bool,
        /// Stop after this many rows.
        #[arg(long)]
        limit: Option<u64>,
        /// Scan this secondary index instead of the primary key.
        #[arg(long)]
        index: Option<String>,
        #[command(flatten)]
        format: FormatArgs,
    },
    /// Scan every key starting with a byte prefix.
    #[command(allow_negative_numbers = true)]
    Prefix {
        /// Database file.
        path: PathBuf,
        /// Table name.
        table: String,
        /// Prefix, in the format given by --key-format.
        prefix: String,
        /// Largest key first.
        #[arg(long)]
        desc: bool,
        /// Stop after this many rows.
        #[arg(long)]
        limit: Option<u64>,
        #[command(flatten)]
        format: FormatArgs,
    },
    /// Count the rows in a key range, without reading any value.
    #[command(allow_negative_numbers = true)]
    Count {
        /// Database file.
        path: PathBuf,
        /// Table name.
        table: String,
        /// Lower bound. Omit for unbounded.
        #[arg(long)]
        from: Option<String>,
        /// Upper bound. Omit for unbounded.
        #[arg(long)]
        to: Option<String>,
        /// Exclude the lower bound.
        #[arg(long)]
        from_exclusive: bool,
        /// Exclude the upper bound.
        #[arg(long)]
        to_exclusive: bool,
        /// Count through this secondary index instead of the primary key.
        #[arg(long)]
        index: Option<String>,
        #[command(flatten)]
        format: FormatArgs,
    },
    /// Build a database from a tab separated input file.
    Build {
        /// Output file. Written to a temporary file and moved into place when complete.
        out: PathBuf,
        /// Table name.
        #[arg(long)]
        table: String,
        /// Key encoding id.
        #[arg(long, default_value = "ascii")]
        encoding: String,
        /// Input file with one `key<TAB>value` row per line, or `-` for stdin.
        #[arg(long)]
        input: PathBuf,
        /// How the input columns are encoded.
        ///
        /// `auto` follows the key encoding: decimal for `i64`, hex for the fixed
        /// width id encodings, and for `ascii` the escaped text these tools print,
        /// where `\\` is one backslash and `\xNN` one byte. `text` reads a key the
        /// same escaped way whatever the encoding. `hex` reads both columns as hex
        /// digits; under the other two the value column is taken as it stands.
        #[arg(long, value_enum, default_value_t = TextFormat::Auto)]
        input_format: TextFormat,
        /// Page size.
        #[arg(long, default_value_t = 4096)]
        page_size: usize,
        /// Store digests in Eytzinger order.
        #[arg(long)]
        eytzinger: bool,
        /// Compress page payloads with the upstream zstd filter.
        #[arg(long)]
        zstd: bool,
        /// Record bytes buffered before the sorter spills to disk.
        #[arg(long, default_value_t = 64 * 1024 * 1024)]
        sort_buffer: usize,
    },
}

#[derive(Args, Debug, Clone)]
struct FormatArgs {
    /// How keys on the command line and in the output are encoded.
    #[arg(long, value_enum, default_value_t = TextFormat::Auto, global = true)]
    key_format: TextFormat,
    /// How values are printed.
    #[arg(long, value_enum, default_value_t = ValueFormat::Auto, global = true)]
    value_format: ValueFormat,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
enum TextFormat {
    /// Decide from the table's key encoding.
    Auto,
    /// Lowercase hex.
    Hex,
    /// Text, with `\\` for a backslash and `\xNN` for one byte.
    Text,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
enum ValueFormat {
    /// Text when the bytes are valid UTF-8 without control characters, hex otherwise.
    Auto,
    /// Lowercase hex.
    Hex,
    /// UTF-8 text, lossily.
    Text,
    /// Just the length in bytes.
    Len,
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("drydb: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Inspect { path } => inspect(&path, cli.budget),
        Command::Verify {
            path,
            no_pages,
            no_trees,
            max_problems,
        } => verify(
            &path,
            cli.budget,
            VerifyOptions {
                check_pages: !no_pages,
                check_trees: !no_trees,
                max_problems,
            },
        ),
        Command::Get {
            path,
            table,
            key,
            index,
            format,
        } => get(&path, cli.budget, &table, index.as_deref(), &key, &format),
        Command::Range {
            path,
            table,
            from,
            to,
            from_exclusive,
            to_exclusive,
            desc,
            limit,
            index,
            format,
        } => range(
            &path,
            cli.budget,
            &table,
            index.as_deref(),
            RangeArgs {
                from,
                to,
                from_exclusive,
                to_exclusive,
                desc,
                limit,
            },
            &format,
        ),
        Command::Prefix {
            path,
            table,
            prefix: pattern,
            desc,
            limit,
            format,
        } => prefix(&path, cli.budget, &table, &pattern, desc, limit, &format),
        Command::Count {
            path,
            table,
            from,
            to,
            from_exclusive,
            to_exclusive,
            index,
            format,
        } => count(
            &path,
            cli.budget,
            &table,
            index.as_deref(),
            RangeArgs {
                from,
                to,
                from_exclusive,
                to_exclusive,
                desc: false,
                limit: None,
            },
            &format,
        ),
        Command::Build {
            out,
            table,
            encoding,
            input,
            input_format,
            page_size,
            eytzinger,
            zstd,
            sort_buffer,
        } => build(BuildArgs {
            out,
            table,
            encoding,
            input,
            input_format,
            page_size,
            eytzinger,
            zstd,
            sort_buffer,
        }),
    }
}

struct RangeArgs {
    from: Option<String>,
    to: Option<String>,
    from_exclusive: bool,
    to_exclusive: bool,
    desc: bool,
    limit: Option<u64>,
}

struct BuildArgs {
    out: PathBuf,
    table: String,
    encoding: String,
    input: PathBuf,
    input_format: TextFormat,
    page_size: usize,
    eytzinger: bool,
    zstd: bool,
    sort_buffer: usize,
}

fn open(path: &PathBuf, budget: u64) -> Result<Database> {
    OpenOptions::new().memory_budget(budget).open(path)
}

fn inspect(path: &PathBuf, budget: u64) -> Result<()> {
    let db = open(path, budget)?;
    let catalog = db.catalog();
    println!("file:            {}", path.display());
    println!("format:          DryDB 1.4");
    println!("page size:       {}", catalog.page_size);
    println!("pages:           {}", catalog.page_count);
    println!(
        "page directory:  offset {}",
        catalog.directory_position.get()
    );
    println!(
        "page filters:    {}",
        if catalog.filters.is_empty() {
            "none".to_string()
        } else {
            catalog.filters.join(", ")
        }
    );
    println!("tables:          {}", catalog.tables.len());
    for table in &catalog.tables {
        println!();
        println!("  table `{}`", table.name);
        println!(
            "    primary key: {} ({}, root {})",
            table.primary.name,
            table.primary.key_encoding_id,
            root_of(&table.primary)
        );
        for index in &table.secondaries {
            println!(
                "    index:       {} ({}, {}, {:?}, root {})",
                index.name,
                index.key_encoding_id,
                if index.is_unique {
                    "unique"
                } else {
                    "non-unique"
                },
                index.value_kind,
                root_of(index)
            );
        }
    }
    Ok(())
}

fn root_of(index: &drydb::IndexDescriptor) -> String {
    match index.root {
        Some(root) => root.to_string(),
        None => "empty".to_string(),
    }
}

fn verify(path: &PathBuf, budget: u64, options: VerifyOptions) -> Result<()> {
    let db = open(path, budget)?;
    let report = db.verify(options)?;
    println!("pages checked:   {}", report.pages_checked);
    println!("trees checked:   {}", report.trees_checked);
    println!("entries checked: {}", report.entries_checked);
    if report.is_ok() {
        println!("result:          ok");
        return Ok(());
    }
    if report.problems.is_empty() {
        // Truncated with nothing recorded: the pass stopped before it could say what it
        // found, which is not the same as finding nothing.
        println!("result:          incomplete");
        return Err(Error::new(
            ErrorKind::CorruptData,
            "verification could not finish; raise `--budget` or lower `--max-problems`",
        ));
    }
    println!("result:          {} problem(s)", report.problems.len());
    for problem in &report.problems {
        println!("  {problem}");
    }
    if report.truncated {
        println!("  ... stopped at the problem limit");
    }
    Err(Error::new(
        ErrorKind::CorruptData,
        format!(
            "{} problem(s) found in {}",
            report.problems.len(),
            path.display()
        ),
    ))
}

fn get(
    path: &PathBuf,
    budget: u64,
    table_name: &str,
    index_name: Option<&str>,
    key: &str,
    format: &FormatArgs,
) -> Result<()> {
    let db = open(path, budget)?;
    let table = db.table(table_name)?;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    match index_name {
        None => {
            let key = parse_key(key, table.key_encoding().as_ref(), format.key_format)?;
            match table.get(&key)? {
                Some(value) => {
                    write_value(&mut out, value.as_bytes(), format.value_format)?;
                    writeln!(out).map_err(io_error)?;
                }
                None => {
                    writeln!(out, "not found").map_err(io_error)?;
                    return Err(Error::new(ErrorKind::InvalidArgument, "key not found"));
                }
            }
        }
        Some(name) => {
            let index = table.index(name)?;
            let key = parse_key(key, index.key_encoding().as_ref(), format.key_format)?;
            let mut cursor = index.lookup(&key)?;
            let mut found = 0u64;
            while cursor.advance()? {
                let value = cursor.value().expect("positioned");
                write_value(&mut out, value.as_bytes(), format.value_format)?;
                writeln!(out).map_err(io_error)?;
                found += 1;
            }
            if found == 0 {
                writeln!(out, "not found").map_err(io_error)?;
                return Err(Error::new(ErrorKind::InvalidArgument, "key not found"));
            }
        }
    }
    Ok(())
}

fn range(
    path: &PathBuf,
    budget: u64,
    table_name: &str,
    index_name: Option<&str>,
    args: RangeArgs,
    format: &FormatArgs,
) -> Result<()> {
    let db = open(path, budget)?;
    let table = db.table(table_name)?;
    let order = if args.desc {
        Order::Descending
    } else {
        Order::Ascending
    };
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut printed = 0u64;

    match index_name {
        None => {
            let encoding = table.key_encoding().clone();
            let from = parse_optional(&args.from, encoding.as_ref(), format.key_format)?;
            let to = parse_optional(&args.to, encoding.as_ref(), format.key_format)?;
            let mut cursor = table.range(
                bound(&from, args.from_exclusive),
                bound(&to, args.to_exclusive),
                order,
            )?;
            // Checked before the step, not after printing: `--limit 0` asks for no rows
            // and has to read none rather than one.
            while args.limit.is_none_or(|n| printed < n) && cursor.advance()? {
                let entry = cursor.current().expect("positioned");
                write!(
                    out,
                    "{}\t",
                    render_key(entry.key(), encoding.as_ref(), format.key_format)
                )
                .map_err(io_error)?;
                write_value(&mut out, entry.value(), format.value_format)?;
                writeln!(out).map_err(io_error)?;
                printed += 1;
            }
        }
        Some(name) => {
            let index = table.index(name)?;
            let encoding = index.key_encoding().clone();
            let from = parse_optional(&args.from, encoding.as_ref(), format.key_format)?;
            let to = parse_optional(&args.to, encoding.as_ref(), format.key_format)?;
            let mut cursor = index.range(
                bound(&from, args.from_exclusive),
                bound(&to, args.to_exclusive),
                order,
            )?;
            while args.limit.is_none_or(|n| printed < n) && cursor.advance()? {
                let value = cursor.value().expect("positioned");
                write!(
                    out,
                    "{}\t",
                    render_key(cursor.key(), encoding.as_ref(), format.key_format)
                )
                .map_err(io_error)?;
                write_value(&mut out, value.as_bytes(), format.value_format)?;
                writeln!(out).map_err(io_error)?;
                printed += 1;
            }
        }
    }
    Ok(())
}

fn prefix(
    path: &PathBuf,
    budget: u64,
    table_name: &str,
    prefix: &str,
    desc: bool,
    limit: Option<u64>,
    format: &FormatArgs,
) -> Result<()> {
    let db = open(path, budget)?;
    let table = db.table(table_name)?;
    let encoding = table.key_encoding().clone();
    let prefix = parse_prefix(prefix, encoding.as_ref(), format.key_format)?;
    let order = if desc {
        Order::Descending
    } else {
        Order::Ascending
    };
    let mut cursor = table.prefix(&prefix, order)?;

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut printed = 0u64;
    // Checked before the step, not after printing: `--limit 0` asks for no rows and has
    // to read none rather than one.
    while limit.is_none_or(|n| printed < n) && cursor.advance()? {
        let entry = cursor.current().expect("positioned");
        write!(
            out,
            "{}\t",
            render_key(entry.key(), encoding.as_ref(), format.key_format)
        )
        .map_err(io_error)?;
        write_value(&mut out, entry.value(), format.value_format)?;
        writeln!(out).map_err(io_error)?;
        printed += 1;
    }
    Ok(())
}

fn count(
    path: &PathBuf,
    budget: u64,
    table_name: &str,
    index_name: Option<&str>,
    args: RangeArgs,
    format: &FormatArgs,
) -> Result<()> {
    let db = open(path, budget)?;
    let table = db.table(table_name)?;
    let total = match index_name {
        None => {
            let encoding = table.key_encoding().clone();
            let from = parse_optional(&args.from, encoding.as_ref(), format.key_format)?;
            let to = parse_optional(&args.to, encoding.as_ref(), format.key_format)?;
            table.count_range(
                bound(&from, args.from_exclusive),
                bound(&to, args.to_exclusive),
            )?
        }
        Some(name) => {
            let index = table.index(name)?;
            let encoding = index.key_encoding().clone();
            let from = parse_optional(&args.from, encoding.as_ref(), format.key_format)?;
            let to = parse_optional(&args.to, encoding.as_ref(), format.key_format)?;
            index.count_range(
                bound(&from, args.from_exclusive),
                bound(&to, args.to_exclusive),
            )?
        }
    };
    println!("{total}");
    Ok(())
}

fn build(args: BuildArgs) -> Result<()> {
    let encoding = encoding_by_id(&args.encoding)?;
    let mut builder = DatabaseBuilder::new()
        .page_size(args.page_size)?
        .eytzinger_digests(args.eytzinger)
        .sort_buffer(args.sort_buffer);
    if args.zstd {
        builder = builder.page_filter(Arc::new(ZstdFilter::default()));
    }
    let table = builder.create_table(args.table.clone(), Arc::clone(&encoding))?;

    let reader: Box<dyn BufRead> = if args.input.as_os_str() == "-" {
        Box::new(std::io::BufReader::new(std::io::stdin()))
    } else {
        Box::new(std::io::BufReader::new(
            std::fs::File::open(&args.input)
                .map_err(|e| Error::new(ErrorKind::Io, format!("cannot open the input: {e}")))?,
        ))
    };

    let mut rows = 0u64;
    for (number, line) in reader.lines().enumerate() {
        let line = line.map_err(|e| {
            Error::new(
                ErrorKind::Io,
                format!("cannot read line {}: {e}", number + 1),
            )
        })?;
        if line.is_empty() {
            continue;
        }
        let (key, value) = line.split_once('\t').ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidArgument,
                format!("line {} has no tab separator", number + 1),
            )
        })?;
        let key = parse_key(key, encoding.as_ref(), args.input_format)?;
        let value = match args.input_format {
            TextFormat::Hex => parse_hex(value)?,
            _ => value.as_bytes().to_vec(),
        };
        builder.append(table, &key, &value)?;
        rows += 1;
    }

    let report = builder.build_to_file(&args.out)?;
    println!(
        "wrote {} ({} rows, {} pages, {} bytes)",
        args.out.display(),
        rows,
        report.page_count,
        report.file_size
    );
    Ok(())
}

fn encoding_by_id(id: &str) -> Result<Arc<dyn KeyEncoding>> {
    match id {
        "i64" => Ok(Arc::new(Int64Encoding)),
        "ascii" => Ok(Arc::new(AsciiEncoding)),
        "uuidv7" => Ok(Arc::new(Uuidv7Encoding)),
        "ulid" => Ok(Arc::new(UlidEncoding)),
        other => Err(Error::new(
            ErrorKind::UnknownEncoding,
            format!("unknown key encoding `{other}`; this build knows i64, ascii, uuidv7, ulid"),
        )),
    }
}

fn bound(key: &Option<Vec<u8>>, exclusive: bool) -> Bound<&[u8]> {
    match key {
        None => Bound::Unbounded,
        Some(k) if exclusive => Bound::Excluded(k.as_slice()),
        Some(k) => Bound::Included(k.as_slice()),
    }
}

fn parse_optional(
    text: &Option<String>,
    encoding: &dyn KeyEncoding,
    format: TextFormat,
) -> Result<Option<Vec<u8>>> {
    text.as_deref()
        .map(|t| parse_key(t, encoding, format))
        .transpose()
}

/// Turns a command line key into bytes.
///
/// `auto` follows the table's encoding: decimal for `i64`, hex for the fixed width id
/// encodings, and raw text for `ascii`.
fn parse_key(text: &str, encoding: &dyn KeyEncoding, format: TextFormat) -> Result<Vec<u8>> {
    let bytes = parse_key_bytes(text, encoding, format)?;
    encoding.validate_key(&bytes)?;
    Ok(bytes)
}

/// The same, for a prefix.
///
/// A prefix is shorter than a key by definition, so the width check a whole key gets
/// would refuse every prefix of a fixed-width encoding.
fn parse_prefix(text: &str, encoding: &dyn KeyEncoding, format: TextFormat) -> Result<Vec<u8>> {
    parse_key_bytes(text, encoding, format)
}

fn parse_key_bytes(text: &str, encoding: &dyn KeyEncoding, format: TextFormat) -> Result<Vec<u8>> {
    match format {
        // The encoding reads back what it writes, so a key copied from a listing can be
        // handed straight back. Guessing here instead meant `uuidv7` printed one form
        // and accepted another.
        TextFormat::Auto => encoding.parse_key(text),
        TextFormat::Hex => parse_hex(text),
        TextFormat::Text => unescape_text(text),
    }
}

fn parse_hex(text: &str) -> Result<Vec<u8>> {
    let text = text.trim();
    if text.len() % 2 != 0 {
        return Err(Error::new(
            ErrorKind::InvalidArgument,
            "hex input must have an even number of digits",
        ));
    }
    let mut out = Vec::with_capacity(text.len() / 2);
    let bytes = text.as_bytes();
    for pair in bytes.chunks(2) {
        let hi = hex_digit(pair[0])?;
        let lo = hex_digit(pair[1])?;
        out.push(hi << 4 | lo);
    }
    Ok(out)
}

fn hex_digit(byte: u8) -> Result<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        other => Err(Error::new(
            ErrorKind::InvalidArgument,
            format!("`{}` is not a hex digit", other as char),
        )),
    }
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn render_key(key: &[u8], encoding: &dyn KeyEncoding, format: TextFormat) -> String {
    match format {
        TextFormat::Auto => encoding.format_key(key),
        TextFormat::Hex => to_hex(key),
        TextFormat::Text => escape_text(key),
    }
}

/// Renders a key as text, escaping what text cannot carry.
///
/// `from_utf8_lossy` turns every byte it cannot read into the same character, so two
/// different keys print the same and neither can be looked up again from what was
/// printed. Valid UTF-8 is left as it is, a backslash is doubled, and anything else is
/// written `\xNN`, which [`unescape_text`] reads back.
fn escape_text(key: &[u8]) -> String {
    let mut out = String::with_capacity(key.len());
    let mut rest = key;
    while !rest.is_empty() {
        let (text, bad) = match std::str::from_utf8(rest) {
            Ok(text) => (text, 0),
            Err(e) => {
                let text = std::str::from_utf8(&rest[..e.valid_up_to()]).expect("valid prefix");
                (text, e.error_len().unwrap_or(rest.len() - e.valid_up_to()))
            }
        };
        for c in text.chars() {
            match c {
                '\\' => out.push_str("\\\\"),
                c if c.is_control() => {
                    let mut buf = [0u8; 4];
                    for &byte in c.encode_utf8(&mut buf).as_bytes() {
                        out.push_str(&format!("\\x{byte:02x}"));
                    }
                }
                c => out.push(c),
            }
        }
        let consumed = text.len();
        for &byte in &rest[consumed..consumed + bad] {
            out.push_str(&format!("\\x{byte:02x}"));
        }
        rest = &rest[consumed + bad..];
    }
    out
}

/// The inverse of [`escape_text`].
fn unescape_text(text: &str) -> Result<Vec<u8>> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'\\' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        match bytes.get(i + 1) {
            Some(b'\\') => {
                out.push(b'\\');
                i += 2;
            }
            Some(b'x') => {
                let digits = bytes.get(i + 2..i + 4).ok_or_else(|| {
                    Error::new(
                        ErrorKind::InvalidArgument,
                        "`\\x` needs two hex digits after it",
                    )
                })?;
                out.push(hex_digit(digits[0])? << 4 | hex_digit(digits[1])?);
                i += 4;
            }
            _ => {
                return Err(Error::new(
                    ErrorKind::InvalidArgument,
                    "a backslash in a key is written `\\\\`, and a byte `\\xNN`",
                ))
            }
        }
    }
    Ok(out)
}

/// Writes a value out in the chosen form, a chunk at a time.
///
/// A value is as large as the file makes it, and rendering one into a `String` first
/// meant holding the value and two characters per byte of it at once: printing a one
/// megabyte value as hex took three megabytes, none of which the memory budget covers,
/// because the printing is the tool's and not the database's.
fn write_value(out: &mut impl Write, value: &[u8], format: ValueFormat) -> Result<()> {
    match format {
        ValueFormat::Len => write!(out, "{}", value.len()).map_err(io_error),
        ValueFormat::Hex => write_hex(out, value),
        ValueFormat::Text => write_text(out, value),
        ValueFormat::Auto => match std::str::from_utf8(value) {
            Ok(text) if !text.chars().any(|c| c.is_control()) => {
                out.write_all(text.as_bytes()).map_err(io_error)
            }
            _ => write_hex(out, value),
        },
    }
}

const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

fn write_hex(out: &mut impl Write, bytes: &[u8]) -> Result<()> {
    let mut buffer = [0u8; 8192];
    for chunk in bytes.chunks(buffer.len() / 2) {
        for (i, &byte) in chunk.iter().enumerate() {
            buffer[i * 2] = HEX_DIGITS[(byte >> 4) as usize];
            buffer[i * 2 + 1] = HEX_DIGITS[(byte & 0x0f) as usize];
        }
        out.write_all(&buffer[..chunk.len() * 2])
            .map_err(io_error)?;
    }
    Ok(())
}

/// Writes the bytes as text, putting the replacement character where they are not UTF-8.
///
/// This is what a value asked for as text has always shown; a value is not handed back
/// to a query, and `auto` prints hex for anything that is not clean text.
fn write_text(out: &mut impl Write, bytes: &[u8]) -> Result<()> {
    let mut rest = bytes;
    while !rest.is_empty() {
        match std::str::from_utf8(rest) {
            Ok(text) => {
                out.write_all(text.as_bytes()).map_err(io_error)?;
                return Ok(());
            }
            Err(e) => {
                let valid = e.valid_up_to();
                out.write_all(&rest[..valid]).map_err(io_error)?;
                out.write_all("\u{fffd}".as_bytes()).map_err(io_error)?;
                let skipped = e.error_len().unwrap_or(rest.len() - valid);
                rest = &rest[valid + skipped..];
            }
        }
    }
    Ok(())
}

fn io_error(e: std::io::Error) -> Error {
    Error::new(ErrorKind::Io, format!("cannot write output: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    thread_local! {
        static ENABLED: Cell<bool> = const { Cell::new(false) };
        static LIVE: Cell<i64> = const { Cell::new(0) };
        static PEAK: Cell<i64> = const { Cell::new(0) };
    }

    struct Counting;

    // SAFETY: every method forwards to the system allocator unchanged; the counters are
    // bookkeeping on the side and never affect the pointers returned.
    #[allow(unsafe_code)]
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let _ = ENABLED.try_with(|on| {
                if on.get() {
                    let _ = LIVE.try_with(|live| {
                        let now = live.get() + layout.size() as i64;
                        live.set(now);
                        let _ = PEAK.try_with(|peak| {
                            if now > peak.get() {
                                peak.set(now);
                            }
                        });
                    });
                }
            });
            unsafe { System.alloc(layout) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            let _ = ENABLED.try_with(|on| {
                if on.get() {
                    let _ = LIVE.try_with(|live| live.set(live.get() - layout.size() as i64));
                }
            });
            unsafe { System.dealloc(ptr, layout) }
        }
    }

    #[global_allocator]
    static ALLOCATOR: Counting = Counting;

    fn peak_of(body: impl FnOnce()) -> i64 {
        LIVE.with(|n| n.set(0));
        PEAK.with(|n| n.set(0));
        ENABLED.with(|on| on.set(true));
        body();
        ENABLED.with(|on| on.set(false));
        PEAK.with(|n| n.get())
    }

    /// Printing used to build the whole rendering first, so a one megabyte value shown
    /// as hex held the value and two characters for each of its bytes at once. What it
    /// costs now does not follow the size of the value.
    #[test]
    fn printing_a_value_does_not_hold_a_copy_of_it() {
        let value = vec![b'x'; 1 << 20];
        let mut sink = std::io::sink();

        for format in [
            ValueFormat::Hex,
            ValueFormat::Text,
            ValueFormat::Auto,
            ValueFormat::Len,
        ] {
            let peak = peak_of(|| write_value(&mut sink, &value, format).unwrap());
            assert!(
                peak < 64 * 1024,
                "printing a {} byte value as {format:?} allocated {peak} bytes",
                value.len()
            );
        }
    }

    /// And what it writes is what it wrote before.
    #[test]
    fn printing_a_value_writes_what_it_always_did() {
        let mut out = Vec::new();
        write_value(&mut out, &[0x00, 0xff, b'a'], ValueFormat::Hex).unwrap();
        assert_eq!(out, b"00ff61");

        let mut out = Vec::new();
        write_value(&mut out, b"plain", ValueFormat::Auto).unwrap();
        assert_eq!(out, b"plain");

        let mut out = Vec::new();
        write_value(&mut out, &[0xff, b'a'], ValueFormat::Auto).unwrap();
        assert_eq!(out, b"ff61", "not clean text, so hex");

        let mut out = Vec::new();
        write_value(&mut out, &[0xff, b'a'], ValueFormat::Text).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "\u{fffd}a");

        let mut out = Vec::new();
        write_value(&mut out, &[1, 2, 3], ValueFormat::Len).unwrap();
        assert_eq!(out, b"3");
    }
}
