//! Filter ids, table descriptors and index descriptors.
//!
//! ```text
//! PageFilter[n]   name_length(u8) name(utf8)
//! Table[n]        name_length(i32) name(utf8)
//!                 IndexDescriptor  (primary key)
//!                 index_count(u16)
//!                 IndexDescriptor[index_count]  (secondary keys)
//! IndexDescriptor name_length(u16) encoding_id_length(u16)
//!                 name(utf8) encoding_id(utf8)
//!                 is_unique(u8) value_kind(u8) root_ordinal(i64, back-patched)
//! ```

use super::{read_i32, read_i64, read_u16, read_u8, FileOffset, PageOrdinal, HEADER_LEN};
use crate::budget::{Charge, BUFFER_OVERHEAD};
use crate::error::{Error, ErrorKind, Result};

/// What an index stores in the value position of its tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueKind {
    /// The record value itself (primary key trees).
    RawData,
    /// The primary key of the record.
    PrimaryKey,
    /// A [`PageRef`](super::pageref::PageRef) pointing at the record value.
    PageRef,
}

impl ValueKind {
    fn from_u8(v: u8) -> Result<ValueKind> {
        match v {
            0 => Ok(ValueKind::RawData),
            1 => Ok(ValueKind::PrimaryKey),
            2 => Ok(ValueKind::PageRef),
            other => Err(Error::new(
                ErrorKind::UnsupportedFormat,
                format!("unknown value kind {other}"),
            )),
        }
    }

    /// On-disk discriminant.
    pub fn to_u8(self) -> u8 {
        match self {
            ValueKind::RawData => 0,
            ValueKind::PrimaryKey => 1,
            ValueKind::PageRef => 2,
        }
    }
}

/// One B+Tree inside a table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexDescriptor {
    /// Index name, unique within the table.
    pub name: String,
    /// Whether the tree rejects duplicate keys. Non-unique trees store `(key, rid)`
    /// composite keys.
    pub is_unique: bool,
    /// Registered id of the key encoding, e.g. `i64` or `ascii`.
    pub key_encoding_id: String,
    /// What the tree stores as its value.
    pub value_kind: ValueKind,
    /// Root page, or `None` for a tree the C# builder left empty.
    pub root: Option<PageOrdinal>,
    /// File offset of the back-patched root field. Used by the builder, and by
    /// `inspect` tooling.
    pub root_field_offset: u64,
}

/// One table: a primary key tree plus zero or more secondary index trees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableDescriptor {
    /// Table name.
    pub name: String,
    /// Primary key tree.
    pub primary: IndexDescriptor,
    /// Secondary index trees, in descriptor order.
    pub secondaries: Vec<IndexDescriptor>,
}

impl TableDescriptor {
    /// Looks a secondary index up by name.
    pub fn secondary(&self, name: &str) -> Option<&IndexDescriptor> {
        self.secondaries.iter().find(|i| i.name == name)
    }
}

/// Everything the header section describes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Catalog {
    /// Page size recorded in the header.
    pub page_size: usize,
    /// Number of page directory slots.
    pub page_count: u64,
    /// Offset of the page directory section.
    pub directory_position: FileOffset,
    /// Page filter ids, in the order the builder applied them on encode.
    pub filters: Vec<String>,
    /// Table descriptors, in file order.
    pub tables: Vec<TableDescriptor>,
    /// Offset just past the last descriptor, i.e. where the first page starts.
    pub descriptor_end: u64,
}

impl Catalog {
    /// Looks a table up by name.
    pub fn table(&self, name: &str) -> Option<&TableDescriptor> {
        self.tables.iter().find(|t| t.name == name)
    }
}

/// Outcome of a catalog parse over a prefix of the file.
#[derive(Debug)]
pub enum CatalogParse {
    /// The prefix contained the whole catalog.
    Complete(Box<Catalog>),
    /// More bytes are needed; at least this many in total.
    NeedMore(usize),
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
    need: Option<usize>,
}

impl<'a> Reader<'a> {
    fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(len)?;
        match self.buf.get(self.pos..end) {
            Some(slice) => {
                self.pos = end;
                Some(slice)
            }
            None => {
                self.need = Some(self.need.unwrap_or(0).max(end));
                None
            }
        }
    }
}

macro_rules! need {
    ($reader:expr, $expr:expr) => {
        match $expr {
            Some(v) => v,
            None => {
                return Ok(CatalogParse::NeedMore(
                    $reader.need.unwrap_or($reader.buf.len() + 1),
                ))
            }
        }
    };
}

/// The memory one element of a growing `Vec` may hold.
///
/// A `Vec` doubles, so a live element can occupy twice its size, and during a growth the
/// old array and the new one exist at the same time. Three times the element size covers
/// both, and one allocation header is added per element rather than per array, which is
/// an over-count that keeps the arithmetic local.
fn slot_cost<T>() -> u64 {
    3 * std::mem::size_of::<T>() as u64 + BUFFER_OVERHEAD
}

/// Reserves what a `String` of `len` bytes costs before it is built.
fn charge_string(charge: &mut Charge, len: usize) -> Result<()> {
    charge.grow(len as u64 + BUFFER_OVERHEAD)
}

/// Parses the catalog out of a prefix of the file that already contains the header.
///
/// `header_page_size`, `header_page_count` and `header_directory_position` come from
/// the validated [`Header`](super::header::Header).
///
/// Every string and every vector slot is reserved on `charge` before it is allocated, so
/// a file whose descriptors do not fit in the budget fails part way through the parse
/// rather than after it. A header claiming more tables than the file holds therefore
/// cannot make the parse allocate past the budget on its way to reporting the truncation.
pub fn parse(
    buf: &[u8],
    page_filter_count: u16,
    table_count: u16,
    page_size: usize,
    page_count: u64,
    directory_position: FileOffset,
    charge: &mut Charge,
) -> Result<CatalogParse> {
    let mut r = Reader {
        buf,
        pos: HEADER_LEN,
        need: None,
    };

    // Capacities come from the parsed data, never from a count field: a header claiming
    // 65535 tables would otherwise reserve megabytes before a single descriptor is read.
    let mut filters = Vec::new();
    for i in 0..page_filter_count {
        let len_byte = need!(r, r.take(1))[0] as usize;
        let name = need!(r, r.take(len_byte));
        charge_string(charge, name.len())?;
        charge.grow(slot_cost::<String>())?;
        filters.push(utf8(name, || format!("page filter {i} id"))?);
    }

    let mut tables = Vec::new();
    for i in 0..table_count {
        let name_len_bytes = need!(r, r.take(4));
        let name_len = super::checked_len(read_i32(name_len_bytes, 0)?, "table name length")?;
        let name_bytes = need!(r, r.take(name_len));
        charge_string(charge, name_bytes.len())?;
        charge.grow(slot_cost::<TableDescriptor>())?;
        let name = utf8(name_bytes, || format!("table {i} name"))?;

        let primary = match read_index_descriptor(&mut r, charge)? {
            Some(d) => d,
            None => return Ok(CatalogParse::NeedMore(r.need.unwrap_or(buf.len() + 1))),
        };

        let count_bytes = need!(r, r.take(2));
        let index_count = read_u16(count_bytes, 0)?;

        let mut secondaries = Vec::new();
        for _ in 0..index_count {
            charge.grow(slot_cost::<IndexDescriptor>())?;
            match read_index_descriptor(&mut r, charge)? {
                Some(d) => secondaries.push(d),
                None => return Ok(CatalogParse::NeedMore(r.need.unwrap_or(buf.len() + 1))),
            }
        }

        for (a, b) in secondaries.iter().enumerate() {
            if secondaries[..a].iter().any(|x| x.name == b.name) {
                return Err(Error::corrupt(format!(
                    "table `{}` declares the index name `{}` twice",
                    describe_name(&name),
                    describe_name(&b.name)
                )));
            }
        }

        tables.push(TableDescriptor {
            name,
            primary,
            secondaries,
        });
    }

    for (a, t) in tables.iter().enumerate() {
        if tables[..a].iter().any(|x| x.name == t.name) {
            return Err(Error::corrupt(format!(
                "duplicate table name `{}`",
                describe_name(&t.name)
            )));
        }
    }

    charge.grow(std::mem::size_of::<Catalog>() as u64 + BUFFER_OVERHEAD)?;
    Ok(CatalogParse::Complete(Box::new(Catalog {
        page_size,
        page_count,
        directory_position,
        filters,
        tables,
        descriptor_end: r.pos as u64,
    })))
}

fn read_index_descriptor(
    r: &mut Reader<'_>,
    charge: &mut Charge,
) -> Result<Option<IndexDescriptor>> {
    let head = match r.take(4) {
        Some(h) => h,
        None => return Ok(None),
    };
    let name_len = read_u16(head, 0)? as usize;
    let encoding_len = read_u16(head, 2)? as usize;

    let name_bytes = match r.take(name_len) {
        Some(b) => b,
        None => return Ok(None),
    };
    charge_string(charge, name_bytes.len())?;
    let name = utf8(name_bytes, || "index name".to_string())?;

    let encoding_bytes = match r.take(encoding_len) {
        Some(b) => b,
        None => return Ok(None),
    };
    charge_string(charge, encoding_bytes.len())?;
    let key_encoding_id = utf8(encoding_bytes, || "key encoding id".to_string())?;

    let tail = match r.take(10) {
        Some(t) => t,
        None => return Ok(None),
    };
    let is_unique_byte = read_u8(tail, 0)?;
    let is_unique = match is_unique_byte {
        0 => false,
        1 => true,
        other => {
            return Err(Error::corrupt(format!(
                "index `{name}` has is_unique = {other}, expected 0 or 1"
            )))
        }
    };
    let value_kind = ValueKind::from_u8(read_u8(tail, 1)?)?;
    let root = PageOrdinal::from_i64(read_i64(tail, 2)?)?;
    let root_field_offset = (r.pos - 8) as u64;

    Ok(Some(IndexDescriptor {
        name,
        is_unique,
        key_encoding_id,
        value_kind,
        root,
        root_field_offset,
    }))
}

/// Bytes of a name an error message may carry.
///
/// A name is as long as the file says it is, and a message that renders one in full is
/// memory spent outside the budget on the way to reporting that the file is damaged.
const DESCRIBE_NAME_BYTES: usize = 48;

/// Renders a name for an error message, bounded.
///
/// Used for names that come out of a file and for names that come from a caller: both
/// are as long as someone else decided, and neither is worth the memory to render whole.
pub(crate) fn describe_name(name: &str) -> String {
    let mut cut = DESCRIBE_NAME_BYTES.min(name.len());
    while !name.is_char_boundary(cut) {
        cut -= 1;
    }
    if cut == name.len() {
        return name.to_string();
    }
    format!("{}... ({} bytes)", &name[..cut], name.len())
}

fn utf8(bytes: &[u8], what: impl Fn() -> String) -> Result<String> {
    std::str::from_utf8(bytes)
        .map(|s| s.to_owned())
        .map_err(|e| Error::corrupt(format!("{} is not valid UTF-8: {e}", what())))
}

/// Encodes an index descriptor. `root_placeholder` is what the builder writes before
/// the tree exists; upstream writes the offset just past the descriptor there.
pub fn encode_index_descriptor(
    name: &str,
    key_encoding_id: &str,
    is_unique: bool,
    value_kind: ValueKind,
    root_placeholder: i64,
) -> Result<Vec<u8>> {
    let name_bytes = name.as_bytes();
    let encoding_bytes = key_encoding_id.as_bytes();
    if name_bytes.len() > u16::MAX as usize {
        return Err(Error::invalid(format!(
            "index name `{name}` is longer than 65535 bytes"
        )));
    }
    if encoding_bytes.len() > u16::MAX as usize {
        return Err(Error::invalid("key encoding id is longer than 65535 bytes"));
    }
    let mut out = Vec::with_capacity(4 + name_bytes.len() + encoding_bytes.len() + 10);
    out.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
    out.extend_from_slice(&(encoding_bytes.len() as u16).to_le_bytes());
    out.extend_from_slice(name_bytes);
    out.extend_from_slice(encoding_bytes);
    out.push(u8::from(is_unique));
    out.push(value_kind.to_u8());
    out.extend_from_slice(&root_placeholder.to_le_bytes());
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::header::Header;

    fn build_bytes() -> (Vec<u8>, Header) {
        let header = Header {
            major_version: 1,
            minor_version: 4,
            page_filter_count: 1,
            page_size: 4096,
            table_count: 1,
            page_count: 2,
            page_directory_position: FileOffset::new(512),
        };
        let mut out = header.encode().to_vec();
        out.push(3);
        out.extend_from_slice(b"zst");
        out.extend_from_slice(&(5i32).to_le_bytes());
        out.extend_from_slice(b"users");
        out.extend_from_slice(
            &encode_index_descriptor("users_pk", "i64", true, ValueKind::RawData, 7).unwrap(),
        );
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(
            &encode_index_descriptor("by_name", "ascii", false, ValueKind::PageRef, 9).unwrap(),
        );
        (out, header)
    }

    fn parse_all(bytes: &[u8], header: &Header) -> Result<CatalogParse> {
        let budget = crate::budget::Budget::new(u64::MAX / 2);
        let mut charge = budget.try_reserve(0).unwrap();
        parse(
            bytes,
            header.page_filter_count,
            header.table_count,
            header.page_size,
            header.page_count,
            header.page_directory_position,
            &mut charge,
        )
    }

    #[test]
    fn parses_a_full_catalog() {
        let (bytes, header) = build_bytes();
        let catalog = match parse_all(&bytes, &header).unwrap() {
            CatalogParse::Complete(c) => *c,
            CatalogParse::NeedMore(_) => panic!("unexpected truncation"),
        };
        assert_eq!(catalog.filters, vec!["zst".to_string()]);
        assert_eq!(catalog.tables.len(), 1);
        let table = &catalog.tables[0];
        assert_eq!(table.name, "users");
        assert_eq!(table.primary.key_encoding_id, "i64");
        assert!(table.primary.is_unique);
        assert_eq!(table.primary.root, Some(PageOrdinal::new(7)));
        assert_eq!(table.secondaries.len(), 1);
        assert_eq!(table.secondaries[0].name, "by_name");
        assert_eq!(table.secondaries[0].value_kind, ValueKind::PageRef);
        assert!(!table.secondaries[0].is_unique);
        assert_eq!(catalog.descriptor_end as usize, bytes.len());
    }

    #[test]
    fn reports_need_more_for_every_prefix() {
        let (bytes, header) = build_bytes();
        for len in HEADER_LEN..bytes.len() {
            match parse_all(&bytes[..len], &header).unwrap() {
                CatalogParse::NeedMore(n) => assert!(n > len, "need {n} must exceed {len}"),
                CatalogParse::Complete(_) => panic!("prefix of {len} bytes parsed as complete"),
            }
        }
    }

    #[test]
    fn rejects_invalid_utf8_names() {
        let (mut bytes, header) = build_bytes();
        let pos = bytes.windows(5).position(|w| w == b"users").unwrap();
        bytes[pos] = 0xff;
        assert_eq!(
            parse_all(&bytes, &header).unwrap_err().kind(),
            ErrorKind::CorruptData
        );
    }

    #[test]
    fn rejects_out_of_range_is_unique() {
        let (mut bytes, header) = build_bytes();
        let pos = bytes.len() - 10;
        bytes[pos] = 42;
        assert_eq!(
            parse_all(&bytes, &header).unwrap_err().kind(),
            ErrorKind::CorruptData
        );
    }

    #[test]
    fn rejects_unknown_value_kind() {
        let (mut bytes, header) = build_bytes();
        // The last descriptor's tail is is_unique(1) value_kind(1) root(8).
        let pos = bytes.len() - 9;
        bytes[pos] = 42;
        assert_eq!(
            parse_all(&bytes, &header).unwrap_err().kind(),
            ErrorKind::UnsupportedFormat
        );
    }

    #[test]
    fn root_field_offset_points_at_the_root_bytes() {
        let (bytes, header) = build_bytes();
        let catalog = match parse_all(&bytes, &header).unwrap() {
            CatalogParse::Complete(c) => *c,
            CatalogParse::NeedMore(_) => panic!(),
        };
        let off = catalog.tables[0].primary.root_field_offset as usize;
        assert_eq!(
            i64::from_le_bytes(bytes[off..off + 8].try_into().unwrap()),
            7
        );
    }
}
