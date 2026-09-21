//! Shared model for the C#/Rust interop harness.
//!
//! One fixture spec drives both implementations: each builds a database from it and
//! each dumps the same queries out of it. Comparing the four combinations
//! (C#→C#, C#→Rust, Rust→C#, Rust→Rust) separates "we can read their files" from
//! "they can read ours" from "we agree on what a query means".

use std::ops::Bound;
use std::sync::Arc;

use base64::Engine;
use drydb::{
    AsciiEncoding, Database, DatabaseBuilder, Int64Encoding, KeyEncoding, Order, UlidEncoding,
    Uuidv7Encoding,
};
use serde::{Deserialize, Serialize};

/// Base64 helper matching what `System.Text.Json` writes.
pub fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Decodes a base64 string from a spec or dump.
pub fn unb64(text: &str) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(text)
        .unwrap_or_else(|e| panic!("invalid base64 in fixture: {e}"))
}

/// A fixture: what to build and what to ask it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Spec {
    /// Page size the builder is configured with.
    pub page_size: usize,
    /// Whether digests are stored in Eytzinger order.
    #[serde(default)]
    pub eytzinger: bool,
    /// Page filter id, or `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    /// Tables to build.
    pub tables: Vec<TableSpec>,
    /// Queries both implementations answer.
    pub queries: Queries,
}

/// One table in a fixture.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TableSpec {
    /// Table name.
    pub name: String,
    /// Primary key encoding id.
    pub encoding: String,
    /// Rows, in append order.
    pub rows: Vec<Row>,
    /// Secondary indexes.
    #[serde(default)]
    pub indexes: Vec<IndexSpec>,
}

/// A row, base64 encoded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Row {
    /// Key bytes.
    pub k: String,
    /// Value bytes.
    pub v: String,
}

/// A secondary index in a fixture.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexSpec {
    /// Index name.
    pub name: String,
    /// Whether duplicate index keys are rejected.
    pub unique: bool,
    /// Index key encoding id.
    pub encoding: String,
    /// How the index key is derived from a row.
    pub key_from: String,
}

/// The query set a dump answers.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Queries {
    /// Point lookups.
    pub points: Vec<PointQuery>,
    /// Range scans.
    pub ranges: Vec<RangeQuery>,
    /// Range counts.
    pub counts: Vec<RangeQuery>,
    /// Single-key index lookups.
    pub index_lookups: Vec<IndexLookup>,
    /// Index range scans.
    pub index_ranges: Vec<IndexRangeQuery>,
}

/// A point lookup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PointQuery {
    /// Table name.
    pub table: String,
    /// Key, base64.
    pub key: String,
}

/// A range scan or count.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RangeQuery {
    /// Table name.
    pub table: String,
    /// Lower bound, base64, or `null` for unbounded.
    pub lower: Option<String>,
    /// Upper bound, base64, or `null` for unbounded.
    pub upper: Option<String>,
    /// Whether the lower bound is exclusive.
    pub lower_exclusive: bool,
    /// Whether the upper bound is exclusive.
    pub upper_exclusive: bool,
    /// `asc` or `desc`. Ignored by counts.
    #[serde(default = "asc")]
    pub order: String,
}

fn asc() -> String {
    "asc".to_string()
}

/// A single-key index lookup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexLookup {
    /// Table name.
    pub table: String,
    /// Index name.
    pub index: String,
    /// Index key, base64.
    pub key: String,
}

/// An index range scan.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexRangeQuery {
    /// Table name.
    pub table: String,
    /// Index name.
    pub index: String,
    /// Lower bound, base64, or `null`.
    pub lower: Option<String>,
    /// Upper bound, base64, or `null`.
    pub upper: Option<String>,
    /// Whether the lower bound is exclusive.
    pub lower_exclusive: bool,
    /// Whether the upper bound is exclusive.
    pub upper_exclusive: bool,
    /// `asc` or `desc`.
    #[serde(default = "asc")]
    pub order: String,
}

/// What both implementations report for a fixture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Dump {
    /// Per-table scans and counts.
    pub tables: Vec<TableDump>,
    /// Point lookup results.
    pub points: Vec<PointResult>,
    /// Range scan results, values only.
    pub ranges: Vec<Vec<String>>,
    /// Range counts.
    pub counts: Vec<i64>,
    /// Index lookup results, values only.
    pub index_lookups: Vec<Vec<String>>,
    /// Index range results, values only.
    pub index_ranges: Vec<Vec<String>>,
}

/// One table's scans.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TableDump {
    /// Table name.
    pub name: String,
    /// Ascending scan.
    pub scan: Vec<Row>,
    /// Descending scan.
    pub scan_descending: Vec<Row>,
    /// Total row count.
    pub count: i64,
}

/// A point lookup result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PointResult {
    /// Whether the key was present.
    pub found: bool,
    /// The value, when found.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub value: Option<String>,
}

/// Resolves an encoding id the same way both implementations do.
pub fn encoding_for(id: &str) -> Arc<dyn KeyEncoding> {
    match id {
        "i64" => Arc::new(Int64Encoding),
        "ascii" => Arc::new(AsciiEncoding),
        "uuidv7" => Arc::new(Uuidv7Encoding),
        "ulid" => Arc::new(UlidEncoding),
        other => panic!("fixture uses unknown encoding `{other}`"),
    }
}

/// Derives a secondary index key, matching `DryDbOracle.DeriveIndexKey`.
pub fn derive_index_key(rule: &str, key: &[u8], value: &[u8]) -> Vec<u8> {
    if rule == "key" {
        return key.to_vec();
    }
    if let Some(n) = rule.strip_prefix("value_prefix:") {
        let n: usize = n.parse().expect("index rule needs a number");
        return value[..n.min(value.len())].to_vec();
    }
    if let Some(n) = rule.strip_prefix("value_suffix:") {
        let n: usize = n.parse().expect("index rule needs a number");
        let take = n.min(value.len());
        return value[value.len() - take..].to_vec();
    }
    if let Some(encoded) = rule.strip_prefix("const:") {
        return unb64(encoded);
    }
    panic!("unknown index key rule `{rule}`")
}

/// Builds a database from a spec with the Rust builder.
pub fn build(spec: &Spec, path: &std::path::Path) -> drydb::Result<()> {
    let mut builder = DatabaseBuilder::new()
        .page_size(spec.page_size)?
        .eytzinger_digests(spec.eytzinger);
    match spec.filter.as_deref() {
        None => {}
        Some("zstd") => builder = builder.page_filter(Arc::new(drydb::ZstdFilter::default())),
        Some(other) => panic!("fixture uses unknown filter `{other}`"),
    }
    for table in &spec.tables {
        let id = builder.create_table(table.name.clone(), encoding_for(&table.encoding))?;
        for index in &table.indexes {
            let rule = index.key_from.clone();
            builder.add_secondary_index(
                id,
                index.name.clone(),
                index.unique,
                encoding_for(&index.encoding),
                Box::new(move |key, value| Ok(derive_index_key(&rule, key, value))),
            )?;
        }
        for row in &table.rows {
            builder.append(id, &unb64(&row.k), &unb64(&row.v))?;
        }
    }
    if path.exists() {
        std::fs::remove_file(path).expect("cannot replace the output file");
    }
    builder.build_to_file(path)?;
    Ok(())
}

fn bound_of(value: &Option<String>, exclusive: bool) -> (Option<Vec<u8>>, bool) {
    (value.as_deref().map(unb64), exclusive)
}

fn as_bound(key: &Option<Vec<u8>>, exclusive: bool) -> Bound<&[u8]> {
    match key {
        None => Bound::Unbounded,
        Some(k) if exclusive => Bound::Excluded(k.as_slice()),
        Some(k) => Bound::Included(k.as_slice()),
    }
}

fn order_of(text: &str) -> Order {
    if text == "desc" {
        Order::Descending
    } else {
        Order::Ascending
    }
}

/// Answers a spec's queries against a database with the Rust reader.
pub fn dump(spec: &Spec, db: &Database) -> drydb::Result<Dump> {
    let mut tables = Vec::new();
    for table_spec in &spec.tables {
        let table = db.table(&table_spec.name)?;
        let mut scan = Vec::new();
        let mut cursor = table.scan(Order::Ascending)?;
        while cursor.advance()? {
            let entry = cursor.current().expect("positioned");
            scan.push(Row {
                k: b64(entry.key()),
                v: b64(entry.value()),
            });
        }
        let mut scan_descending = Vec::new();
        let mut cursor = table.scan(Order::Descending)?;
        while cursor.advance()? {
            let entry = cursor.current().expect("positioned");
            scan_descending.push(Row {
                k: b64(entry.key()),
                v: b64(entry.value()),
            });
        }
        let count = table.count()? as i64;
        tables.push(TableDump {
            name: table_spec.name.clone(),
            scan,
            scan_descending,
            count,
        });
    }

    let mut points = Vec::new();
    for query in &spec.queries.points {
        let table = db.table(&query.table)?;
        match table.get(&unb64(&query.key))? {
            Some(value) => points.push(PointResult {
                found: true,
                value: Some(b64(value.as_bytes())),
            }),
            None => points.push(PointResult {
                found: false,
                value: None,
            }),
        }
    }

    let mut ranges = Vec::new();
    for query in &spec.queries.ranges {
        let table = db.table(&query.table)?;
        let (lower, lower_ex) = bound_of(&query.lower, query.lower_exclusive);
        let (upper, upper_ex) = bound_of(&query.upper, query.upper_exclusive);
        let mut cursor = table.range(
            as_bound(&lower, lower_ex),
            as_bound(&upper, upper_ex),
            order_of(&query.order),
        )?;
        let mut values = Vec::new();
        while cursor.advance()? {
            values.push(b64(cursor.current().expect("positioned").value()));
        }
        ranges.push(values);
    }

    let mut counts = Vec::new();
    for query in &spec.queries.counts {
        let table = db.table(&query.table)?;
        let (lower, lower_ex) = bound_of(&query.lower, query.lower_exclusive);
        let (upper, upper_ex) = bound_of(&query.upper, query.upper_exclusive);
        counts.push(
            table.count_range(as_bound(&lower, lower_ex), as_bound(&upper, upper_ex))? as i64,
        );
    }

    let mut index_lookups = Vec::new();
    for query in &spec.queries.index_lookups {
        let table = db.table(&query.table)?;
        let index = table.index(&query.index)?;
        let mut cursor = index.lookup(&unb64(&query.key))?;
        let mut values = Vec::new();
        while cursor.advance()? {
            values.push(b64(cursor.value().expect("positioned").as_bytes()));
        }
        index_lookups.push(values);
    }

    let mut index_ranges = Vec::new();
    for query in &spec.queries.index_ranges {
        let table = db.table(&query.table)?;
        let index = table.index(&query.index)?;
        let (lower, lower_ex) = bound_of(&query.lower, query.lower_exclusive);
        let (upper, upper_ex) = bound_of(&query.upper, query.upper_exclusive);
        let mut cursor = index.range(
            as_bound(&lower, lower_ex),
            as_bound(&upper, upper_ex),
            order_of(&query.order),
        )?;
        let mut values = Vec::new();
        while cursor.advance()? {
            values.push(b64(cursor.value().expect("positioned").as_bytes()));
        }
        index_ranges.push(values);
    }

    Ok(Dump {
        tables,
        points,
        ranges,
        counts,
        index_lookups,
        index_ranges,
    })
}

/// SHA-256 of a file, for the fixture manifest.
pub fn file_sha256(path: &std::path::Path) -> String {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path).expect("cannot hash a file that was not written");
    let digest = Sha256::digest(&bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}
