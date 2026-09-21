//! MessagePack values for [`drydb`], interoperable with MessagePack-CSharp.
//!
//! A value is one MessagePack document. There is no envelope and no type tag: the
//! database stores bytes, and the caller says what type they hold. That is what lets the
//! same table be read from C# with `MessagePackReadOnlyTable<T>`.
//!
//! # Which layout matches which C# DTO
//!
//! MessagePack-CSharp writes a DTO either as an array of values, keyed by integer, or as
//! a map keyed by property name, depending on how the type is declared. Pick the
//! [`Layout`] that matches:
//!
//! | [`Layout`] | C# declaration |
//! | --- | --- |
//! | [`Layout::Array`] | `[MessagePackObject]` with integer `[Key]` attributes |
//! | [`Layout::Map`] | `[MessagePackObject(keyAsPropertyName: true)]`, or `ContractlessStandardResolver` |
//!
//! With [`Layout::Array`], the order of the fields in the Rust struct has to match the
//! `[Key]` numbering on the C# side, because nothing in the bytes says which field is
//! which. With [`Layout::Map`], the names have to match instead.
//!
//! # What this does not claim
//!
//! Not every C# DTO round trips. Integer widths, `null` versus a missing key,
//! MessagePack extension types and custom formatters all have to line up, and this crate
//! does not verify that for a type it has never seen. `docs/compatibility.md` records
//! which fixture types have been checked against the C# implementation.

#![deny(unsafe_code)]
#![warn(missing_docs)]
#![warn(missing_debug_implementations)]
#![warn(rust_2018_idioms)]

use std::ops::Bound;

use drydb::{Charged, Error, ErrorKind, Order, Result, Table, ValueGuard};
use serde::de::DeserializeOwned;
use serde::Serialize;

/// How a struct's fields are written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Layout {
    /// An array of field values, positional. Matches `[MessagePackObject]` with integer
    /// keys.
    #[default]
    Array,
    /// A map from field name to value. Matches `keyAsPropertyName: true`.
    Map,
}

/// Encodes and decodes MessagePack values.
#[derive(Debug, Clone)]
pub struct MessagePackCodec {
    layout: Layout,
    max_value_bytes: usize,
}

impl Default for MessagePackCodec {
    fn default() -> Self {
        MessagePackCodec {
            layout: Layout::Array,
            max_value_bytes: 64 << 20,
        }
    }
}

impl MessagePackCodec {
    /// A codec writing the array layout.
    pub fn new() -> MessagePackCodec {
        MessagePackCodec::default()
    }

    /// A codec writing `layout`.
    pub fn with_layout(layout: Layout) -> MessagePackCodec {
        MessagePackCodec {
            layout,
            ..MessagePackCodec::default()
        }
    }

    /// Largest value this codec will encode or decode.
    pub fn max_value_bytes(mut self, bytes: usize) -> Self {
        self.max_value_bytes = bytes;
        self
    }

    /// The layout this codec writes.
    pub fn layout(&self) -> Layout {
        self.layout
    }

    /// Encodes one value.
    pub fn serialize<T: Serialize>(&self, value: &T) -> Result<Vec<u8>> {
        let bytes = match self.layout {
            Layout::Array => rmp_serde::to_vec(value),
            Layout::Map => rmp_serde::to_vec_named(value),
        }
        .map_err(|e| {
            Error::new(
                ErrorKind::SchemaMismatch,
                format!("MessagePack encoding failed: {e}"),
            )
        })?;
        if bytes.len() > self.max_value_bytes {
            return Err(Error::new(
                ErrorKind::ValueTooLarge,
                format!(
                    "encoded value is {} bytes, above the {} byte limit",
                    bytes.len(),
                    self.max_value_bytes
                ),
            ));
        }
        Ok(bytes)
    }

    /// Decodes one value.
    pub fn deserialize<T: DeserializeOwned>(&self, bytes: &[u8]) -> Result<T> {
        if bytes.len() > self.max_value_bytes {
            return Err(Error::new(
                ErrorKind::ValueTooLarge,
                format!(
                    "value is {} bytes, above the {} byte limit",
                    bytes.len(),
                    self.max_value_bytes
                ),
            ));
        }
        rmp_serde::from_slice(bytes).map_err(|e| {
            Error::new(
                ErrorKind::SchemaMismatch,
                format!("MessagePack decoding failed: {e}"),
            )
        })
    }
}

/// A table whose values are MessagePack documents of one type.
///
/// Decoding allocates: unlike the rkyv codec, MessagePack has no in-place representation
/// to borrow, so every row read through here builds an owned value. The raw
/// [`ValueGuard`] is still reachable through [`MessagePackTable::table`] for callers that
/// want the bytes.
#[derive(Debug, Clone)]
pub struct MessagePackTable {
    table: Table,
    codec: MessagePackCodec,
}

/// Rows a collected range hands back: each one a key and its decoded value, with the
/// memory the list and the keys take reserved until the whole is dropped.
pub type Rows<T> = Charged<Vec<(Vec<u8>, T)>>;

impl MessagePackTable {
    /// Wraps a table.
    pub fn new(table: Table, codec: MessagePackCodec) -> MessagePackTable {
        MessagePackTable { table, codec }
    }

    /// The underlying table.
    pub fn table(&self) -> &Table {
        &self.table
    }

    /// The codec in use.
    pub fn codec(&self) -> &MessagePackCodec {
        &self.codec
    }

    /// Looks one key up and decodes it.
    pub fn get<T: DeserializeOwned>(&self, key: &[u8]) -> Result<Option<T>> {
        match self.table.get(key)? {
            None => Ok(None),
            Some(value) => self.codec.deserialize(value.as_bytes()).map(Some),
        }
    }

    /// Looks one key up and returns the raw bytes.
    pub fn get_raw(&self, key: &[u8]) -> Result<Option<ValueGuard>> {
        self.table.get(key)
    }

    /// Decodes a bounded range of rows.
    ///
    /// `limit` bounds what this holds in memory; a scan of unknown size belongs on a
    /// [`Cursor`](drydb::Cursor), which holds one page at a time.
    ///
    /// The list and the copies of the keys are reserved against the database's memory
    /// budget as they are made, so a `limit` the budget cannot hold comes back as
    /// [`ErrorKind::BudgetExceeded`](drydb::ErrorKind) rather than being allocated, and
    /// the reservation is released when the returned value is dropped. What a decoded
    /// value allocates inside itself is `T`'s own doing and is not counted here: a type
    /// that owns a megabyte of `String` costs a megabyte this crate cannot see.
    pub fn collect_range<T: DeserializeOwned>(
        &self,
        lower: Bound<&[u8]>,
        upper: Bound<&[u8]>,
        order: Order,
        limit: usize,
    ) -> Result<Rows<T>> {
        let slot = std::mem::size_of::<(Vec<u8>, T)>() as u64;
        // One reservation for the whole collection: emptying the list drops the rows and
        // keeps the allocation, so what pays for the list cannot live in the rows.
        let mut charge = self.table.reserve(0)?;
        let mut cursor = self.table.range(lower, upper, order)?;
        let mut rows: Vec<(Vec<u8>, T)> = Vec::new();
        while rows.len() < limit && cursor.advance()? {
            let entry = cursor.current().expect("positioned");
            let more = self
                .table
                .reserve(slot + entry.key().len() as u64 + drydb::BUFFER_OVERHEAD)?;
            charge.absorb(more);
            if rows.len() == rows.capacity() {
                // Growing the list holds the old allocation and the new one at once
                // while the rows move across.
                let _moving = self.table.reserve(rows.len() as u64 * slot)?;
                rows.reserve_exact(1);
            }
            let value: T = self.codec.deserialize(entry.value())?;
            rows.push((entry.key_to_vec(), value));
        }
        Ok(Charged::new(rows, charge))
    }

    /// Calls `f` for every row in a range, decoding one at a time.
    ///
    /// Nothing accumulates: this is the streaming form of
    /// [`MessagePackTable::collect_range`].
    pub fn for_each_range<T, F>(
        &self,
        lower: Bound<&[u8]>,
        upper: Bound<&[u8]>,
        order: Order,
        mut f: F,
    ) -> Result<u64>
    where
        T: DeserializeOwned,
        F: FnMut(&[u8], T) -> Result<()>,
    {
        let mut cursor = self.table.range(lower, upper, order)?;
        let mut seen = 0u64;
        while cursor.advance()? {
            let entry = cursor.current().expect("positioned");
            let value: T = self.codec.deserialize(entry.value())?;
            f(entry.key(), value)?;
            seen += 1;
        }
        Ok(seen)
    }
}
