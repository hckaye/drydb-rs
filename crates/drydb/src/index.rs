//! Secondary index queries.
//!
//! A secondary index tree stores a 16-byte [`PageRef`] in the value position, pointing
//! at the record bytes in the primary tree (or at a blob page). Resolving one costs the
//! page it points at, which is why a secondary cursor materialises its current row
//! rather than borrowing it.
//!
//! Non-unique indexes make duplicate keys unique by appending a 4-byte record id, so a
//! lookup for key `k` is really the composite range `[k||0, k||i32::MAX]`. Record ids run
//! from zero in primary key order within each index key, which is what upstream produces
//! too. Exclusive bounds are translated to that composite space so that "everything above
//! `k`" really excludes every row with key `k`; `docs/compatibility.md` records where
//! upstream differs.

use std::ops::Bound;
use std::sync::Arc;

use crate::btree::Tree;
use crate::budget::Charge;
use crate::encoding::{DuplicateKeyEncoding, EncodingRegistry, KeyEncoding, RID_LEN};
use crate::error::{Error, ErrorKind, Result};
use crate::format::catalog::{IndexDescriptor, ValueKind};
use crate::format::pageref::{PageRef, PAGE_REF_LEN};
use crate::page::ValueGuard;
use crate::query::{count_range, prefix_range, Cursor, KeyRange, Order};
use crate::store::PageStore;

pub(crate) struct IndexInner {
    descriptor: IndexDescriptor,
    tree: Arc<Tree>,
    source_encoding: Arc<dyn KeyEncoding>,
    store: Arc<PageStore>,
    /// Keeps the catalog's reservation alive for as long as this handle holds a copy of
    /// its descriptor, which can be past the end of the `Database`.
    _catalog_charge: Arc<crate::budget::Charge>,
}

impl IndexInner {
    pub(crate) fn build(
        descriptor: &IndexDescriptor,
        store: &Arc<PageStore>,
        encodings: &EncodingRegistry,
        validate_digests: bool,
        catalog_charge: &Arc<crate::budget::Charge>,
    ) -> Result<IndexInner> {
        if descriptor.value_kind != ValueKind::PageRef {
            return Err(Error::new(
                ErrorKind::UnsupportedFormat,
                format!(
                    "secondary index `{}` stores {:?}; this reader resolves PageRef indexes only",
                    descriptor.name, descriptor.value_kind
                ),
            ));
        }
        let source_encoding = encodings.resolve(&descriptor.key_encoding_id)?;
        let tree_encoding: Arc<dyn KeyEncoding> = if descriptor.is_unique {
            Arc::clone(&source_encoding)
        } else {
            Arc::new(DuplicateKeyEncoding::new(Arc::clone(&source_encoding)))
        };
        Ok(IndexInner {
            descriptor: descriptor.clone(),
            tree: Arc::new(Tree::new(
                Arc::clone(store),
                descriptor.root,
                tree_encoding,
                validate_digests,
                Arc::clone(catalog_charge),
            )),
            source_encoding,
            store: Arc::clone(store),
            _catalog_charge: Arc::clone(catalog_charge),
        })
    }
}

/// A secondary index over a table.
#[derive(Clone)]
pub struct Index {
    inner: Arc<IndexInner>,
}

impl std::fmt::Debug for Index {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Index")
            .field("name", &self.inner.descriptor.name)
            .field("unique", &self.inner.descriptor.is_unique)
            .field("encoding", &self.inner.source_encoding.id())
            .finish()
    }
}

impl Index {
    pub(crate) fn tree(&self) -> &Arc<Tree> {
        &self.inner.tree
    }

    pub(crate) fn new(inner: Arc<IndexInner>) -> Index {
        Index { inner }
    }

    /// Index name.
    pub fn name(&self) -> &str {
        &self.inner.descriptor.name
    }

    /// Whether the index rejects duplicate keys.
    pub fn is_unique(&self) -> bool {
        self.inner.descriptor.is_unique
    }

    /// The key encoding callers pass keys in. For a non-unique index this is the
    /// *source* encoding; the record id suffix is internal.
    pub fn key_encoding(&self) -> &Arc<dyn KeyEncoding> {
        &self.inner.source_encoding
    }

    /// The index descriptor.
    pub fn descriptor(&self) -> &IndexDescriptor {
        &self.inner.descriptor
    }

    /// The first value indexed under `key`, or `None`.
    ///
    /// On a non-unique index this is the row with the lowest record id. Record ids are
    /// assigned in primary key order, not in the order rows were appended, so this is
    /// the matching row whose primary key sorts first.
    pub fn get(&self, key: &[u8]) -> Result<Option<ValueGuard>> {
        let mut cursor = self.lookup(key)?;
        if cursor.advance()? {
            Ok(cursor.take_value())
        } else {
            Ok(None)
        }
    }

    /// Every value indexed under `key`, in record id order.
    pub fn lookup(&self, key: &[u8]) -> Result<IndexCursor> {
        self.inner.source_encoding.validate_key(key)?;
        self.range(Bound::Included(key), Bound::Included(key), Order::Ascending)
    }

    /// A cursor over an index key range.
    pub fn range(
        &self,
        lower: Bound<&[u8]>,
        upper: Bound<&[u8]>,
        order: Order,
    ) -> Result<IndexCursor> {
        let charge = self.reserve_bounds(lower, upper)?;
        let range = self.tree_range(lower, upper, true)?;
        let cursor = Cursor::new(Arc::clone(&self.inner.tree), charge, range, order)?;
        Ok(IndexCursor {
            cursor,
            tree: Arc::clone(&self.inner.tree),
            store: Arc::clone(&self.inner.store),
            unique: self.inner.descriptor.is_unique,
            key: Vec::new(),
            key_charge: self.inner.tree.store().budget().try_reserve(0)?,
            value: None,
        })
    }

    /// A cursor over every entry.
    pub fn scan(&self, order: Order) -> Result<IndexCursor> {
        self.range(Bound::Unbounded, Bound::Unbounded, order)
    }

    /// A cursor over every index key starting with `prefix`.
    pub fn prefix(&self, prefix: &[u8], order: Order) -> Result<IndexCursor> {
        if !self.inner.source_encoding.is_byte_lexicographic() {
            return Err(Error::invalid(format!(
                "encoding `{}` does not order keys by their bytes, so a byte prefix does not \
                 describe a key range; use `range` instead",
                self.inner.source_encoding.id()
            )));
        }
        // Charged twice over: once for the prefix range, once for the composite keys the
        // tree range makes out of it. Both are live at the same time.
        let prefix_charge =
            self.reserve_bounds(Bound::Included(prefix), Bound::Included(prefix))?;
        let source = prefix_range(prefix);
        let charge = self.reserve_bounds(
            crate::query::as_ref_bound(&source.lower),
            crate::query::as_ref_bound(&source.upper),
        )?;
        let range = self.tree_range(
            crate::query::as_ref_bound(&source.lower),
            crate::query::as_ref_bound(&source.upper),
            false,
        )?;
        // The copies go before the reservation that stood for them, not after: between
        // the two the budget would say the memory is free while it is still held, and
        // building the cursor below reserves in exactly that window.
        drop(source);
        drop(prefix_charge);
        let cursor = Cursor::for_prefix(Arc::clone(&self.inner.tree), charge, range, order)?;
        Ok(IndexCursor {
            cursor,
            tree: Arc::clone(&self.inner.tree),
            store: Arc::clone(&self.inner.store),
            unique: self.inner.descriptor.is_unique,
            key: Vec::new(),
            key_charge: self.inner.tree.store().budget().try_reserve(0)?,
            value: None,
        })
    }

    /// Counts index entries in a range, without resolving any value.
    pub fn count_range(&self, lower: Bound<&[u8]>, upper: Bound<&[u8]>) -> Result<u64> {
        let _charge = self.reserve_bounds(lower, upper)?;
        let range = self.tree_range(lower, upper, true)?;
        if range.is_empty() {
            return Ok(0);
        }
        count_range(
            &self.inner.tree,
            crate::query::as_ref_bound(&range.lower),
            crate::query::as_ref_bound(&range.upper),
        )
    }

    /// Counts every index entry.
    pub fn count(&self) -> Result<u64> {
        count_range(&self.inner.tree, Bound::Unbounded, Bound::Unbounded)
    }

    /// Reserves what the owned copies of these bounds will cost.
    ///
    /// A non-unique index appends a record id to each bound, so the copy is four bytes
    /// longer than the caller's key.
    fn reserve_bounds(&self, lower: Bound<&[u8]>, upper: Bound<&[u8]>) -> Result<Charge> {
        let extra = if self.inner.descriptor.is_unique {
            0
        } else {
            RID_LEN as u64
        };
        crate::query::reserve_bounds(&self.inner.tree, lower, upper, extra)
    }

    /// Whether these bounds select nothing before they are widened.
    fn is_empty_range(&self, lower: Bound<&[u8]>, upper: Bound<&[u8]>) -> Result<bool> {
        let (Some(lo), Some(hi)) = (bound_ref(lower), bound_ref(upper)) else {
            return Ok(false);
        };
        if self.inner.source_encoding.compare(lo, hi)? != std::cmp::Ordering::Equal {
            return Ok(false);
        }
        Ok(matches!(lower, Bound::Excluded(_)) || matches!(upper, Bound::Excluded(_)))
    }

    /// Translates caller bounds into the tree's key space.
    ///
    /// `validate` is off for prefix bounds, which are shorter than a key by definition.
    fn tree_range(
        &self,
        lower: Bound<&[u8]>,
        upper: Bound<&[u8]>,
        validate: bool,
    ) -> Result<KeyRange> {
        if validate {
            for key in [bound_ref(lower), bound_ref(upper)].into_iter().flatten() {
                self.inner.source_encoding.validate_key(key)?;
            }
        }
        if self.inner.descriptor.is_unique {
            return Ok(KeyRange::new(lower, upper));
        }
        // An empty range stays empty. Widening `k` into `(k, 0)..=(k, i32::MAX)` turns
        // bounds that merely meet, which select nothing, into bounds that cross, which
        // is what a reversed range looks like; the primary tree answers the same query
        // with no rows, and so should this.
        if self.is_empty_range(lower, upper)? {
            return Ok(KeyRange::empty());
        }
        // Composite keys sort by (source key, record id). "Everything above k" has to
        // clear the whole run of record ids for k, and "everything below k" must not
        // reach any of them.
        let lower = match lower {
            Bound::Unbounded => Bound::Unbounded,
            Bound::Included(k) => Bound::Included(DuplicateKeyEncoding::encode(k, 0)),
            Bound::Excluded(k) => Bound::Excluded(DuplicateKeyEncoding::encode(k, i32::MAX)),
        };
        let upper = match upper {
            Bound::Unbounded => Bound::Unbounded,
            Bound::Included(k) => Bound::Included(DuplicateKeyEncoding::encode(k, i32::MAX)),
            Bound::Excluded(k) => Bound::Excluded(DuplicateKeyEncoding::encode(k, 0)),
        };
        Ok(KeyRange::from_bounds(lower, upper))
    }
}

fn bound_ref(bound: Bound<&[u8]>) -> Option<&[u8]> {
    match bound {
        Bound::Unbounded => None,
        Bound::Included(k) | Bound::Excluded(k) => Some(k),
    }
}

/// A cursor over a secondary index.
///
/// Each step resolves the stored [`PageRef`] into the record bytes, which means loading
/// the page it points at. The row is materialised into the cursor (the key is copied,
/// the value is an owning guard), so the two pages involved are not both borrowed at
/// once.
pub struct IndexCursor {
    cursor: Cursor,
    tree: Arc<Tree>,
    store: Arc<PageStore>,
    unique: bool,
    key: Vec<u8>,
    /// What `key` costs. A cursor is a caller's object and a caller can hold many.
    key_charge: Charge,
    value: Option<ValueGuard>,
}

impl std::fmt::Debug for IndexCursor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IndexCursor")
            .field("cursor", &self.cursor)
            .field("unique", &self.unique)
            .finish()
    }
}

impl IndexCursor {
    /// Moves to the next index entry.
    pub fn advance(&mut self) -> Result<bool> {
        // The inner cursor undoes its own failures, but resolving the reference an entry
        // points at happens after it has already moved, and that read can fail for a
        // reason the caller can do something about. Put it back, so a retry returns the
        // row that could not be read rather than the one after it.
        let saved = self.cursor.position();
        match self.advance_inner() {
            Ok(more) => Ok(more),
            Err(e) => {
                self.cursor.restore(saved);
                self.key.clear();
                self.value = None;
                Err(e)
            }
        }
    }

    fn advance_inner(&mut self) -> Result<bool> {
        self.value = None;
        if !self.cursor.advance()? {
            self.key.clear();
            return Ok(false);
        }
        // Borrowing the cursor and writing into `self.key` are disjoint field borrows, so
        // the index key is copied once rather than through a temporary.
        let reference = {
            let entry = self
                .cursor
                .current()
                .ok_or_else(|| Error::corrupt("index cursor advanced without an entry"))?;
            let raw = entry.value();
            if raw.len() != PAGE_REF_LEN {
                return Err(Error::corrupt(format!(
                    "index entry value is {} bytes, expected a {PAGE_REF_LEN} byte PageRef",
                    raw.len()
                )));
            }
            let reference = PageRef::parse(raw)?;
            let key = entry.key();
            let source_key = if self.unique {
                key
            } else {
                DuplicateKeyEncoding::split(key)?.0
            };
            // Reserved before the buffer is grown, not after. `self.cursor`, `self.key`
            // and `self.key_charge` are disjoint fields, so this borrows alongside the
            // entry reference above.
            crate::query::charge_buffer(
                &self.tree,
                &mut self.key_charge,
                &mut self.key,
                source_key.len(),
            )?;
            self.key.clear();
            self.key.extend_from_slice(source_key);
            reference
        };

        let pin = self.store.page(reference.page)?;
        let range = reference.range(pin.len())?;
        self.value = Some(pin.into_guard(range)?);
        Ok(true)
    }

    /// The current index key, without the internal record id.
    pub fn key(&self) -> &[u8] {
        &self.key
    }

    /// The current record value.
    pub fn value(&self) -> Option<&ValueGuard> {
        self.value.as_ref()
    }

    /// Takes the current value, leaving the cursor positioned.
    pub fn take_value(&mut self) -> Option<ValueGuard> {
        self.value.take()
    }
}
