//! B+Tree search.
//!
//! Every page carries an order-preserving digest per entry, so a search first finds the
//! rank of the first digest that can match (binary search over the sorted array, or a
//! branch-free descent over the Eytzinger tree) and only then compares keys, walking
//! the run of equal digests. On `OmittedKeys` pages the digest is exact and there are
//! no key bytes at all, so the comparison *is* the digest comparison.
//!
//! This is the same resolution order the upstream C# reader uses on its SIMD path, so
//! the two agree entry for entry -- including where digests collide.

use std::cmp::Ordering;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::Arc;

use crate::budget::Charge;
use crate::encoding::KeyEncoding;
use crate::error::{Error, Result};
use crate::format::node::{LeafEntry, NodeKind, PageView};
use crate::format::PageOrdinal;
use crate::page::PagePin;
use crate::store::PageStore;

/// Which entry a leaf search is after.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SearchOp {
    /// The entry equal to the key.
    Equal,
    /// The first entry `>= key`.
    LowerBound,
    /// The first entry `> key`.
    UpperBound,
}

/// Hands out the tree identities that mark a page as already checked.
///
/// Starts at one: zero means "checked by nobody", which is the state a freshly read page
/// is in.
static NEXT_TREE_ID: AtomicU64 = AtomicU64::new(1);

/// A single B+Tree, rooted at one page.
pub(crate) struct Tree {
    store: Arc<PageStore>,
    root: Option<PageOrdinal>,
    encoding: Arc<dyn KeyEncoding>,
    /// Identifies this tree in the per-page "already checked" marker.
    id: u64,
    /// Whether a page's digests are checked against its keys before it is used.
    validate_digests: bool,
    /// What the catalog and the handles built from it cost.
    ///
    /// A cursor keeps its tree alive and nothing else, so without this the reservation
    /// that paid for the tree would go when the database did, while the tree the cursor
    /// is reading from is still here.
    _catalog_charge: Arc<Charge>,
}

impl Drop for Tree {
    fn drop(&mut self) {
        // The store keeps this tree's root alive; without this it would stay charged to
        // the budget with nothing left that could ever ask for it.
        self.store.release_root(self.id);
    }
}

impl Tree {
    pub(crate) fn new(
        store: Arc<PageStore>,
        root: Option<PageOrdinal>,
        encoding: Arc<dyn KeyEncoding>,
        validate_digests: bool,
        catalog_charge: Arc<Charge>,
    ) -> Tree {
        Tree {
            store,
            root,
            encoding,
            id: NEXT_TREE_ID.fetch_add(1, AtomicOrdering::Relaxed),
            validate_digests,
            _catalog_charge: catalog_charge,
        }
    }

    /// Loads a page belonging to this tree, checked.
    ///
    /// Searches trust the digest array: it decides where in a page the key comparison
    /// starts, so a digest that disagrees with its key makes a search skip rows without
    /// any bound being violated. The format layer cannot check that, because a digest is
    /// only meaningful next to the key encoding.
    ///
    /// The check is recorded on the page buffer rather than done inside the load, so a
    /// page this tree has not checked is checked here even when something else put it in
    /// the cache first: the same bytes are reachable through blob reads, `verify`, and
    /// every other tree in the file. The marker is cleared when the buffer is evicted,
    /// and it is keyed by tree because the same page under a different key encoding is a
    /// different question.
    fn tree_page(&self, ordinal: PageOrdinal) -> Result<PagePin> {
        let pin = self.store.page(ordinal)?;
        self.check_page(&pin)?;
        Ok(pin)
    }

    /// Runs the digest check on `pin` unless this tree has already run it.
    fn check_page(&self, pin: &PagePin) -> Result<()> {
        if !self.validate_digests || pin.digests_checked_by(self.id) {
            return Ok(());
        }
        let view = pin.view()?;
        self.check_digests(&view)
            .map_err(|e| e.at_page(pin.ordinal().get()))?;
        pin.mark_digests_checked(self.id);
        Ok(())
    }

    /// The key of the `index`-th entry, whichever kind of node this is.
    fn entry_key<'a>(&self, view: &PageView<'a>, index: usize) -> Result<&'a [u8]> {
        match view.kind() {
            NodeKind::Leaf => {
                let entry = view.leaf_entry(index)?;
                view.leaf_key(&entry)
            }
            NodeKind::Internal => {
                let entry = view.internal_entry(index)?;
                view.internal_key(&entry)
            }
        }
    }

    fn digest_mismatch(&self, index: usize, key: &[u8], stored: u64) -> Result<Error> {
        let computed = self.encoding.digest(key)?;
        Ok(Error::corrupt(format!(
            "entry {index} stores digest {stored:#018x} but its key digests to \
             {computed:#018x}; searches would skip rows on this page"
        )))
    }

    /// Checks that a page is ordered the way a search assumes it is.
    ///
    /// A search does two things on a page, and loses rows without violating any bound if
    /// either assumption is wrong. It binary searches the digests to find where to start
    /// comparing keys, and it then scans forward until it meets a key above the one it
    /// wants. So each stored digest has to be the digest of its own key, and the keys
    /// have to be strictly ascending: keys that share a digest can be out of order among
    /// themselves while every digest still agrees with its own key.
    ///
    /// On a page that omits key bytes there is no key to compare a digest against, and
    /// the digest *is* the key column, so the digests are checked for order directly
    /// instead. Where the keys are present, ascending keys already imply ascending
    /// digests, because a digest is order preserving; checking the digests again there
    /// would reject the non-unique index pages of the C# builder, whose digests cover the
    /// record id and are not monotonic across a carry (`docs/compatibility.md`, D1).
    pub(crate) fn check_digests<'a>(&self, view: &PageView<'a>) -> Result<()> {
        if view.layout().eytzinger {
            return self.check_eytzinger_digests(view);
        }
        if view.layout().omitted_keys {
            // A page with no key bytes is read entirely through its digests: the search
            // compares them instead of keys, and everything that wants a key rebuilds it
            // from one. That only means anything for an encoding that can do the rebuild,
            // and says how large the result can be. Under any other encoding the digest
            // is not the key, so a search would match a key that is not there and hand
            // back the row it landed on.
            self.rebuilt_key_capacity()?;
            // Equal digests are two rows under one key. That is normal for a non-unique
            // index, whose digest covers the source key and not the record id, and it is
            // a duplicate key anywhere else: a search stops at the first match and the
            // rows behind it are on the page but out of reach.
            let strict = self.encoding.is_digest_exact();
            let mut previous: Option<u64> = None;
            for index in 0..view.entry_count() {
                let stored = view.digest_at(index)?;
                if let Some(previous) = previous {
                    if stored < previous || (strict && stored == previous) {
                        return Err(Self::digest_out_of_order(index, previous, stored));
                    }
                }
                previous = Some(stored);
            }
            return Ok(());
        }
        let mut previous_key: Option<&'a [u8]> = None;
        for index in 0..view.entry_count() {
            let stored = view.digest_at(index)?;
            let key = self.entry_key(view, index)?;
            self.check_stored_key(index, key)?;
            if !self.encoding.accepts_digest(key, stored)? {
                return Err(self.digest_mismatch(index, key, stored)?);
            }
            if let Some(previous) = previous_key {
                self.check_ascending(index, previous, key)?;
            }
            previous_key = Some(key);
        }
        Ok(())
    }

    /// Checks a stored key against the encoding, the way a caller's key is checked.
    ///
    /// A digest covers at most the first eight bytes of a key, so a key that lost the
    /// rest of itself still digests to what the page says. The width is the part of the
    /// key the digest cannot speak for.
    fn check_stored_key(&self, index: usize, key: &[u8]) -> Result<()> {
        self.encoding.validate_key(key).map_err(|e| {
            Error::corrupt(format!(
                "entry {index} is not a well-formed key of encoding `{}`: {e}",
                self.encoding.id()
            ))
        })
    }

    fn digest_out_of_order(index: usize, previous: u64, stored: u64) -> Error {
        let how = if stored == previous {
            "repeats"
        } else {
            "is below"
        };
        Error::corrupt(format!(
            "entry {index} stores digest {stored:#018x}, which {how} the previous entry's \
             {previous:#018x}; the search over the digests would skip rows"
        ))
    }

    fn check_ascending(&self, index: usize, previous: &[u8], key: &[u8]) -> Result<()> {
        if self.encoding.compare(previous, key)? != Ordering::Less {
            return Err(Error::corrupt(format!(
                "entry {index} ({}) does not sort above the previous entry ({}); a search \
                 stops at the first key above the one it wants, so rows after this one \
                 would be unreachable",
                crate::encoding::describe_key(self.encoding.as_ref(), key),
                crate::encoding::describe_key(self.encoding.as_ref(), previous)
            )));
        }
        Ok(())
    }

    /// The Eytzinger form of the same check.
    ///
    /// The slots are a complete binary tree in BFS order whose in-order traversal is the
    /// entries in sorted order, padded at the end with `u64::MAX`. Walking that traversal
    /// pairs each entry with its slot, and the padding has to be the sentinel because the
    /// descent treats it as larger than any key.
    ///
    /// The tree is complete, so the walk needs no stack: a node's parent is `i / 2` and
    /// it is a right child exactly when `i` is odd.
    fn check_eytzinger_digests<'a>(&self, view: &PageView<'a>) -> Result<()> {
        let complete = view.digest_slot_count();
        if complete == 0 {
            return Ok(());
        }
        let count = view.entry_count();
        // Start at the leftmost node, which is the first in-order position.
        let mut node = 1usize;
        while node * 2 <= complete {
            node *= 2;
        }
        let mut rank = 0usize;
        let mut previous_key: Option<&'a [u8]> = None;
        loop {
            let stored = view.digest_slot(node - 1)?;
            if rank < count {
                let key = self.entry_key(view, rank)?;
                self.check_stored_key(rank, key)?;
                if !self.encoding.accepts_digest(key, stored)? {
                    return Err(self.digest_mismatch(rank, key, stored)?);
                }
                if let Some(previous) = previous_key {
                    self.check_ascending(rank, previous, key)?;
                }
                previous_key = Some(key);
            } else if stored != u64::MAX {
                return Err(Error::corrupt(format!(
                    "Eytzinger slot {} pads past the {count} entries but holds \
                     {stored:#018x} instead of the sentinel; the descent would take the \
                     wrong branch",
                    node - 1
                )));
            }
            rank += 1;
            // Step to the in-order successor: the leftmost node of the right subtree,
            // or the first ancestor this node is in the left subtree of.
            let right = node * 2 + 1;
            if right <= complete {
                node = right;
                while node * 2 <= complete {
                    node *= 2;
                }
            } else {
                while node > 1 && node % 2 == 1 {
                    node /= 2;
                }
                if node == 1 {
                    break;
                }
                node /= 2;
            }
        }
        if rank != complete {
            return Err(Error::corrupt(format!(
                "Eytzinger traversal visited {rank} of {complete} slots"
            )));
        }
        Ok(())
    }

    /// The root page, from the pin when there is one.
    fn root_page(&self) -> Result<Option<PagePin>> {
        let Some(root) = self.root else {
            return Ok(None);
        };
        if let Some(pin) = self.store.retained_root(self.id) {
            return Ok(Some(pin));
        }
        let pin = self.tree_page(root)?;
        // Kept by the store rather than here, so that a later read which needs the
        // memory more can take it back. A tree that held its own root would keep it
        // past that point, and no amount of evicting would reach it.
        if self.store.has_headroom_for(pin.len() as u64) {
            self.store.retain_root(self.id, pin.clone());
        }
        Ok(Some(pin))
    }

    /// The root page, only if it is already pinned or cached.
    fn root_page_cached(&self) -> Result<Option<PagePin>> {
        if let Some(pin) = self.store.retained_root(self.id) {
            return Ok(Some(pin));
        }
        let Some(root) = self.root else {
            return Ok(None);
        };
        let Some(pin) = self.store.page_cached(root) else {
            return Ok(None);
        };
        self.check_page(&pin)?;
        Ok(Some(pin))
    }

    pub(crate) fn store(&self) -> &Arc<PageStore> {
        &self.store
    }

    /// The root page ordinal, or `None` for an empty tree.
    pub(crate) fn root(&self) -> Option<PageOrdinal> {
        self.root
    }

    /// Loads a page without the digest check.
    ///
    /// For `verify`, which reports a bad page and carries on instead of stopping at the
    /// first one, and therefore runs the check itself.
    pub(crate) fn raw_page(&self, ordinal: PageOrdinal) -> Result<PagePin> {
        self.store.page(ordinal)
    }

    pub(crate) fn encoding(&self) -> &Arc<dyn KeyEncoding> {
        &self.encoding
    }

    /// Digest of a search key.
    pub(crate) fn digest_of(&self, key: &[u8]) -> Result<u64> {
        self.encoding.digest(key)
    }

    /// Walks from the root to the leaf that could hold `key`.
    ///
    /// The flag says the descent had to settle for a child that starts strictly below
    /// the key, so the first match may be on a later leaf. See [`Tree::internal_child_index`].
    fn descend(&self, key: &[u8], key_digest: u64) -> Result<Option<(PagePin, bool)>> {
        let Some(root_pin) = self.root_page()? else {
            return Ok(None);
        };
        let mut ordinal = root_pin.ordinal();
        let mut current = Some(root_pin);
        let mut conservative = false;
        let max_depth = self.store.limits().max_tree_depth;
        for _ in 0..max_depth {
            let pin = match current.take() {
                Some(pin) => pin,
                None => self.tree_page(ordinal)?,
            };
            let view = pin.view()?;
            match view.kind() {
                NodeKind::Leaf => return Ok(Some((pin.clone(), conservative))),
                NodeKind::Internal => {
                    if view.entry_count() == 0 {
                        return Err(
                            Error::corrupt("internal page has no children").at_page(ordinal.get())
                        );
                    }
                    let (index, inexact) = self.internal_child_index(&view, key, key_digest)?;
                    conservative |= inexact;
                    let entry = view.internal_entry(index)?;
                    if entry.child == ordinal {
                        return Err(
                            Error::corrupt("internal page points at itself").at_page(ordinal.get())
                        );
                    }
                    ordinal = entry.child;
                }
            }
        }
        Err(Error::corrupt(format!(
            "tree is deeper than the {max_depth} level limit; the child chain is cyclic"
        )))
    }

    /// Whether separators on this page can only be compared through their digests.
    ///
    /// A page that omits key bytes has nothing but the digest to compare, and some
    /// encodings map several distinct keys to one digest on purpose: a non-unique
    /// secondary index digests the source key alone, so every row sharing a source key
    /// shares a digest. Two children can then hold the same digest without holding the
    /// same key.
    pub(crate) fn separators_are_inexact(&self, view: &PageView<'_>) -> bool {
        view.layout().omitted_keys && !self.encoding.is_digest_exact()
    }

    /// Index of the child to descend into, and whether the choice was conservative.
    ///
    /// Normally the last separator `<= key`, which is the child that holds it.
    ///
    /// When the separators can only be compared through their digests, an equal
    /// comparison no longer means the child starts at the key: the child before it can
    /// hold matching rows in its tail while starting below the key, and no rightward walk
    /// reaches a child that was already passed. So the descent takes the last child whose
    /// separator is *strictly* below the key. That child is at or before the first match,
    /// and walking right from it reaches the rest. Upstream instead takes the last child
    /// whose separator compares equal, which is where it loses the rows before it.
    fn internal_child_index(
        &self,
        view: &PageView<'_>,
        key: &[u8],
        key_digest: u64,
    ) -> Result<(usize, bool)> {
        let count = view.entry_count();
        let mut i = view.lower_bound_rank(key_digest)?.min(count);
        // Walk forward to the first separator that is not below the key.
        while i < count && self.compare_internal(view, i, key, key_digest)? == Ordering::Less {
            i += 1;
        }
        if self.separators_are_inexact(view) {
            let inexact =
                i < count && self.compare_internal(view, i, key, key_digest)? == Ordering::Equal;
            return Ok((i.saturating_sub(1), inexact));
        }
        if i < count && self.compare_internal(view, i, key, key_digest)? == Ordering::Equal {
            return Ok((i, false));
        }
        Ok((i.saturating_sub(1), false))
    }

    fn compare_internal(
        &self,
        view: &PageView<'_>,
        index: usize,
        key: &[u8],
        key_digest: u64,
    ) -> Result<Ordering> {
        if view.layout().omitted_keys {
            return Ok(view.digest_at(index)?.cmp(&key_digest));
        }
        let entry = view.internal_entry(index)?;
        let entry_key = view.internal_key(&entry)?;
        self.encoding.compare(entry_key, key)
    }

    fn compare_leaf(
        &self,
        view: &PageView<'_>,
        index: usize,
        key: &[u8],
        key_digest: u64,
    ) -> Result<Ordering> {
        if view.layout().omitted_keys {
            return Ok(view.digest_at(index)?.cmp(&key_digest));
        }
        let entry = view.leaf_entry(index)?;
        let entry_key = view.leaf_key(&entry)?;
        self.encoding.compare(entry_key, key)
    }

    /// Resolves `op` inside one leaf page.
    pub(crate) fn leaf_search(
        &self,
        view: &PageView<'_>,
        key: &[u8],
        key_digest: u64,
        op: SearchOp,
    ) -> Result<Option<usize>> {
        let count = view.entry_count();
        let start = view.lower_bound_rank(key_digest)?.min(count);
        match op {
            SearchOp::Equal => {
                let mut i = start;
                while i < count {
                    match self.compare_leaf(view, i, key, key_digest)? {
                        Ordering::Equal => return Ok(Some(i)),
                        Ordering::Greater => break,
                        Ordering::Less => i += 1,
                    }
                }
                Ok(None)
            }
            SearchOp::LowerBound => {
                let mut i = start;
                while i < count && self.compare_leaf(view, i, key, key_digest)? == Ordering::Less {
                    i += 1;
                }
                Ok(if i < count { Some(i) } else { None })
            }
            SearchOp::UpperBound => {
                let mut i = start;
                while i < count && self.compare_leaf(view, i, key, key_digest)? != Ordering::Greater
                {
                    i += 1;
                }
                Ok(if i < count { Some(i) } else { None })
            }
        }
    }

    /// Descends and resolves `op`, walking right when the bound is not satisfied on the
    /// leaf the descent reached.
    ///
    /// The walk continues rather than stopping at the first sibling: a run of equal keys
    /// can span several leaves, and then the first entry of the next leaf does not
    /// satisfy the bound either. It is limited by the number of pages in the file, so a
    /// cyclic sibling chain is reported instead of followed.
    ///
    /// An exact search normally stops on the leaf the descent reached, because that is
    /// the only leaf the key can be on. It walks too when the descent had to settle for
    /// an earlier child, and then stops as soon as a leaf ends at or above the key.
    pub(crate) fn search(&self, key: &[u8], op: SearchOp) -> Result<Option<(PagePin, usize)>> {
        let key_digest = self.digest_of(key)?;
        let Some((mut pin, conservative)) = self.descend(key, key_digest)? else {
            return Ok(None);
        };
        let mut steps_left = self.store.page_count().saturating_add(1);
        loop {
            let right = {
                let view = pin.view()?;
                if let Some(index) = self.leaf_search(&view, key, key_digest, op)? {
                    return Ok(Some((pin.clone(), index)));
                }
                if op == SearchOp::Equal
                    && !(conservative && self.ends_below(&view, key, key_digest)?)
                {
                    return Ok(None);
                }
                view.right_sibling()
            };
            let Some(right) = right else {
                // Nothing further right. Either the tree ends on this leaf and no entry
                // satisfies the bound, or the chain stops short and an empty result
                // would stand in for the entries beyond the break.
                if !self.ends_at(pin.ordinal(), true)? {
                    return Err(Error::corrupt(format!(
                        "the leaf chain stops at page {} but the tree does not end there",
                        pin.ordinal()
                    ))
                    .at_page(pin.ordinal().get()));
                }
                return Ok(None);
            };
            if steps_left == 0 {
                return Err(Error::corrupt(
                    "leaf sibling chain visits more pages than the file contains; it is cyclic",
                ));
            }
            steps_left -= 1;
            pin = self.step_sibling(pin.ordinal(), right, true)?;
        }
    }

    /// Whether every entry on this leaf is below `key`, so a match can only be further
    /// right. An empty leaf counts, since it rules nothing out.
    fn ends_below(&self, view: &PageView<'_>, key: &[u8], key_digest: u64) -> Result<bool> {
        let count = view.entry_count();
        if count == 0 {
            return Ok(true);
        }
        Ok(self.compare_leaf(view, count - 1, key, key_digest)? == Ordering::Less)
    }

    /// Point lookup.
    pub(crate) fn find(&self, key: &[u8]) -> Result<Option<(PagePin, usize)>> {
        self.search(key, SearchOp::Equal)
    }

    /// Point lookup against cached pages only.
    ///
    /// Returns `Ok(None)` when a page on the path is not cached, which the caller has to
    /// tell apart from "the key is not there".
    pub(crate) fn find_cached(&self, key: &[u8]) -> Result<Option<Option<(PagePin, usize)>>> {
        if self.root.is_none() {
            return Ok(Some(None));
        }
        let key_digest = self.digest_of(key)?;
        let Some(root_pin) = self.root_page_cached()? else {
            return Ok(None);
        };
        let mut ordinal = root_pin.ordinal();
        let mut current = Some(root_pin);
        let mut conservative = false;
        let mut steps_left = self.store.page_count().saturating_add(1);
        let max_depth = self.store.limits().max_tree_depth;
        // Counted per page read on the way down, leaf included, the same way `descend`
        // counts: the two have to agree about which trees are too deep to search.
        let mut depth_left = max_depth;
        // Only the way down counts against the depth. Stepping right across leaves is a
        // different walk with its own bound, and counting it here turned a wide tree
        // into a tree that looked too deep.
        let mut descending = true;
        loop {
            if descending {
                if depth_left == 0 {
                    return Err(Error::corrupt(format!(
                        "tree is deeper than the {max_depth} level limit; the child chain \
                         is cyclic"
                    )));
                }
                depth_left -= 1;
            }
            let pin = match current.take() {
                Some(pin) => pin,
                None => match self.store.page_cached(ordinal) {
                    Some(pin) => {
                        self.check_page(&pin)?;
                        pin
                    }
                    None => return Ok(None),
                },
            };
            let view = pin.view()?;
            match view.kind() {
                NodeKind::Leaf => {
                    descending = false;
                    if let Some(index) =
                        self.leaf_search(&view, key, key_digest, SearchOp::Equal)?
                    {
                        return Ok(Some(Some((pin.clone(), index))));
                    }
                    if !(conservative && self.ends_below(&view, key, key_digest)?) {
                        return Ok(Some(None));
                    }
                    let Some(right) = view.right_sibling() else {
                        return Ok(Some(None));
                    };
                    if steps_left == 0 {
                        return Err(Error::corrupt(
                            "leaf sibling chain visits more pages than the file contains; \
                             it is cyclic",
                        ));
                    }
                    steps_left -= 1;
                    ordinal = right;
                }
                NodeKind::Internal => {
                    if view.entry_count() == 0 {
                        return Err(
                            Error::corrupt("internal page has no children").at_page(ordinal.get())
                        );
                    }
                    let (index, inexact) = self.internal_child_index(&view, key, key_digest)?;
                    conservative |= inexact;
                    let entry = view.internal_entry(index)?;
                    if entry.child == ordinal {
                        return Err(
                            Error::corrupt("internal page points at itself").at_page(ordinal.get())
                        );
                    }
                    ordinal = entry.child;
                }
            }
        }
    }

    /// The leftmost leaf entry, or `None` for an empty tree.
    pub(crate) fn min_leaf(&self) -> Result<Option<(PagePin, usize)>> {
        self.edge_leaf(true)
    }

    /// The rightmost leaf entry, or `None` for an empty tree.
    pub(crate) fn max_leaf(&self) -> Result<Option<(PagePin, usize)>> {
        self.edge_leaf(false)
    }

    /// Whether `ordinal` is the leaf the chain ends at, going this way.
    ///
    /// A chain that ends because a page has no sibling has either reached the end of the
    /// tree or lost its way: the page the tree itself ends at settles which. Without
    /// this, a scan over a file whose chain is broken stops early and says it is done,
    /// which is a short answer given as a whole one.
    pub(crate) fn ends_at(&self, ordinal: PageOrdinal, ascending: bool) -> Result<bool> {
        let edge = if ascending {
            self.max_leaf()?
        } else {
            self.min_leaf()?
        };
        Ok(match edge {
            Some((pin, _)) => pin.ordinal() == ordinal,
            // An empty tree has no leaves to reach, so nothing can have reached past one.
            None => true,
        })
    }

    fn edge_leaf(&self, leftmost: bool) -> Result<Option<(PagePin, usize)>> {
        let Some(root_pin) = self.root_page()? else {
            return Ok(None);
        };
        let mut ordinal = root_pin.ordinal();
        let mut current = Some(root_pin);
        let max_depth = self.store.limits().max_tree_depth;
        for _ in 0..max_depth {
            let pin = match current.take() {
                Some(pin) => pin,
                None => self.tree_page(ordinal)?,
            };
            let view = pin.view()?;
            let count = view.entry_count();
            match view.kind() {
                NodeKind::Leaf => {
                    if count == 0 {
                        return Ok(None);
                    }
                    let index = if leftmost { 0 } else { count - 1 };
                    return Ok(Some((pin.clone(), index)));
                }
                NodeKind::Internal => {
                    if count == 0 {
                        return Err(
                            Error::corrupt("internal page has no children").at_page(ordinal.get())
                        );
                    }
                    let entry = view.internal_entry(if leftmost { 0 } else { count - 1 })?;
                    if entry.child == ordinal {
                        return Err(
                            Error::corrupt("internal page points at itself").at_page(ordinal.get())
                        );
                    }
                    ordinal = entry.child;
                }
            }
        }
        Err(Error::corrupt(format!(
            "tree is deeper than the {max_depth} level limit; the child chain is cyclic"
        )))
    }

    /// Steps from one leaf to the next along the chain, checking that the page stepped
    /// into points back at the one stepped from.
    ///
    /// The chain is doubly linked, and a walk that follows one direction alone cannot
    /// tell a chain that skips a page from one that does not: the pages it does visit
    /// are in order and it ends where the tree ends, so it hands back part of the table
    /// as though it were all of it. The page it arrives at is loaded either way, so
    /// looking at the link it came by costs nothing.
    pub(crate) fn step_sibling(
        &self,
        from: PageOrdinal,
        to: PageOrdinal,
        ascending: bool,
    ) -> Result<PagePin> {
        let pin = self.leaf_page(to)?;
        let back = {
            let view = pin.view()?;
            if ascending {
                view.left_sibling()
            } else {
                view.right_sibling()
            }
        };
        if back != Some(from) {
            return Err(Error::corrupt(format!(
                "leaf page {to} does not point back at page {from}, so the chain skips a page"
            ))
            .at_page(to.get()));
        }
        Ok(pin)
    }

    /// Loads a sibling leaf, checking that it really is a leaf.
    pub(crate) fn leaf_page(&self, ordinal: PageOrdinal) -> Result<PagePin> {
        let pin = self.tree_page(ordinal)?;
        if pin.view()?.kind() != NodeKind::Leaf {
            return Err(
                Error::corrupt("leaf sibling chain points at an internal page")
                    .at_page(ordinal.get()),
            );
        }
        Ok(pin)
    }

    /// Writes the key of a leaf entry into `buf` when the page omits key bytes.
    ///
    /// Returns `true` when the key was rebuilt into `buf`, `false` when the caller
    /// should read it straight from the page.
    /// The buffer a rebuilt key needs, so a caller can reserve it before the rebuild
    /// writes into it.
    ///
    /// An encoding that does not say refuses the page rather than allocating first and
    /// finding out afterwards, which is the one thing the memory budget cannot cover.
    pub(crate) fn rebuilt_key_capacity(&self) -> Result<usize> {
        self.encoding.max_rebuilt_key_len().ok_or_else(|| {
            Error::corrupt(format!(
                "page omits key bytes but encoding `{}` does not say how large a rebuilt \
                 key can be; implement `KeyEncoding::max_rebuilt_key_len`",
                self.encoding.id()
            ))
        })
    }

    pub(crate) fn rebuild_key(
        &self,
        view: &PageView<'_>,
        index: usize,
        buf: &mut Vec<u8>,
    ) -> Result<bool> {
        if !view.layout().omitted_keys {
            return Ok(false);
        }
        let digest = view.digest_at(index)?;
        self.encoding
            .decode_key_from_digest(digest, buf)
            .map_err(|e| {
                Error::corrupt(format!(
                    "page omits key bytes and encoding `{}` cannot rebuild them: {e}",
                    self.encoding.id()
                ))
            })?;
        Ok(true)
    }
}

/// Resolves a leaf entry's value, but only if every page it needs is already cached.
pub(crate) fn resolve_value_cached(
    store: &PageStore,
    leaf: &PagePin,
    entry: &LeafEntry,
) -> Result<Option<(PagePin, std::ops::Range<usize>)>> {
    match entry.value {
        crate::format::node::LeafValue::Inline { offset, len } => {
            Ok(Some((leaf.clone(), offset..offset + len)))
        }
        crate::format::node::LeafValue::Overflow { page } => match store.page_cached(page) {
            None => Ok(None),
            // Resolve through the pin we already hold: asking the store for the page
            // again could find it evicted and read it, which this path promises not to
            // do.
            Some(pin) => store.blob_from_pin(pin).map(Some),
        },
    }
}

/// Resolves a leaf entry's value into a page pin plus a byte range.
pub(crate) fn resolve_value(
    store: &PageStore,
    leaf: &PagePin,
    entry: &LeafEntry,
) -> Result<(PagePin, std::ops::Range<usize>)> {
    match entry.value {
        crate::format::node::LeafValue::Inline { offset, len } => {
            Ok((leaf.clone(), offset..offset + len))
        }
        crate::format::node::LeafValue::Overflow { page } => store.blob(page),
    }
}
