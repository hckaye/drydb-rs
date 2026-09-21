//! Whole-file verification.
//!
//! Opening a database checks the header, the descriptors and the pages it actually
//! touches. `verify` is the explicit, streaming pass over everything else: every page in
//! the directory, and every tree walked level by level. It holds one page at a time, so
//! it runs inside the same memory budget as a query.
//!
//! A tree is walked one level at a time rather than by descending into children: every
//! page of a level is on that level's sibling chain, so the walk reaches all of them
//! while holding a single page. Descending would either hold one page per level or need
//! a queue the width of the tree.

use crate::btree::Tree;
use crate::budget::{Charge, Charged};
use crate::db::Database;
use crate::error::{Error, ErrorKind, Result};
use crate::format::node::{LeafValue, NodeKind, PageView};
use crate::format::pageref::PageRef;
use crate::format::PageOrdinal;
use crate::page::PagePin;
use crate::store::PageStore;
use std::sync::Arc;

/// What to check.
#[derive(Debug, Clone, Copy)]
pub struct VerifyOptions {
    /// Load and structurally validate every page in the directory.
    pub check_pages: bool,
    /// Walk every tree, checking key order and sibling links.
    pub check_trees: bool,
    /// Stop after this many problems.
    pub max_problems: usize,
}

impl Default for VerifyOptions {
    fn default() -> Self {
        VerifyOptions {
            check_pages: true,
            check_trees: true,
            max_problems: 100,
        }
    }
}

/// One thing that is wrong with the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyProblem {
    /// Page the problem was found on, when known.
    pub page: Option<u64>,
    /// What is wrong.
    pub message: String,
}

impl std::fmt::Display for VerifyProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.page {
            Some(p) => write!(f, "page {p}: {}", self.message),
            None => write!(f, "{}", self.message),
        }
    }
}

/// The outcome of a verification pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VerifyReport {
    /// Pages loaded and validated.
    pub pages_checked: u64,
    /// Tree entries walked.
    pub entries_checked: u64,
    /// Trees walked.
    pub trees_checked: u64,
    /// Problems found, up to `max_problems`.
    pub problems: Vec<VerifyProblem>,
    /// Whether the scan stopped early because `max_problems` was reached.
    pub truncated: bool,
}

impl VerifyReport {
    /// Whether the file passed.
    ///
    /// A truncated pass did not finish looking, so it did not find that the file is
    /// sound; it found that it stopped. Either the caller's `max_problems` was reached,
    /// which means there were problems, or there was no room to record one.
    pub fn is_ok(&self) -> bool {
        self.problems.is_empty() && !self.truncated
    }
}

impl Database {
    /// Runs a streaming verification pass.
    ///
    /// Structural damage is reported in the returned [`VerifyReport`]; only failures
    /// that prevent the pass itself from running (I/O errors, a budget too small to
    /// hold one page) come back as `Err`.
    ///
    /// Turning both passes off is refused rather than answered with an empty report: a
    /// report that nothing is wrong, from a pass that looked at nothing, reads as a
    /// clean file.
    ///
    /// The report is memory the caller holds, so it comes back with the reservation
    /// that paid for it. Dropping it gives the budget its bytes back; the report itself
    /// reads through the [`Charged`] as it would on its own.
    pub fn verify(&self, options: VerifyOptions) -> Result<Charged<VerifyReport>> {
        if !options.check_pages && !options.check_trees {
            return Err(Error::new(
                ErrorKind::InvalidArgument,
                "verification with both the page sweep and the tree walk turned off \
                 would check nothing; leave one of them on",
            ));
        }
        let store = Arc::clone(self.store());
        // Named before the report so that it is dropped after it: a pass that stops on
        // an error drops both here, and the reservation has to be the second to go.
        let mut report_charge = store.budget().try_reserve(0)?;
        let mut report = VerifyReport::default();
        // What stopped the pass rather than being wrong with the file. Checked after
        // each stage, because a pass that could not run has not cleared the file.
        let mut failure: Option<Error> = None;

        if options.check_pages {
            let mut truncated = false;
            {
                let reporter = &mut Reporter {
                    options: &options,
                    label: "page",
                    report: &mut report,
                    charge: &mut report_charge,
                    store: &store,
                    failure: &mut failure,
                };
                for ordinal in 0..self.store().page_count() {
                    if reporter.full() {
                        // Stopping because the pass could not go on is not the same as
                        // stopping because the caller asked for no more findings.
                        truncated = !reporter.stopped();
                        break;
                    }
                    match self.store().page(PageOrdinal::new(ordinal)) {
                        Ok(_) => reporter.report.pages_checked += 1,
                        Err(e) => reporter.failed(Some(ordinal), e),
                    }
                }
            }
            if let Some(e) = failure {
                return Err(e);
            }
            if truncated {
                report.truncated = true;
                return Ok(Charged::new(report, report_charge));
            }
        }

        if options.check_trees {
            // Cloned, not copied: the names can be as long as the file says, and the
            // catalog already holds them.
            let catalog = Arc::clone(self.catalog_arc());
            for descriptor in &catalog.tables {
                let table = self.table(&descriptor.name)?;
                // Only a table with an index needs its overflow pages remembered, and
                // the bits are one per page of the file: a table without one should not
                // have to find room for them.
                let mut overflow = if descriptor.secondaries.is_empty() {
                    None
                } else {
                    Some(OverflowPages::new(&store)?)
                };
                verify_tree(
                    table.tree(),
                    match &mut overflow {
                        Some(overflow) => ValueKind::Record(overflow),
                        None => ValueKind::Plain,
                    },
                    &format!("table `{}` primary key", short_name(&descriptor.name)),
                    &options,
                    &mut report,
                    &mut report_charge,
                    &store,
                    &mut failure,
                )?;
                if let Some(e) = failure {
                    return Err(e);
                }
                if report.problems.len() >= options.max_problems {
                    report.truncated = true;
                    return Ok(Charged::new(report, report_charge));
                }
                for index in &descriptor.secondaries {
                    // The index tree is reachable through the same descriptor; walking
                    // it uses the index's own (possibly composite) encoding.
                    let overflow = overflow.as_ref().expect("a table with an index");
                    verify_index(
                        &table,
                        &index.name,
                        overflow,
                        &options,
                        &mut report,
                        &mut report_charge,
                        &store,
                        &mut failure,
                    )?;
                    if let Some(e) = failure {
                        return Err(e);
                    }
                    if report.problems.len() >= options.max_problems {
                        report.truncated = true;
                        return Ok(Charged::new(report, report_charge));
                    }
                }
            }
        }

        Ok(Charged::new(report, report_charge))
    }
}

#[allow(clippy::too_many_arguments)]
fn verify_index(
    table: &crate::db::Table,
    index_name: &str,
    overflow: &OverflowPages,
    options: &VerifyOptions,
    report: &mut VerifyReport,
    report_charge: &mut Charge,
    store: &Arc<PageStore>,
    failure: &mut Option<Error>,
) -> Result<()> {
    let index = table.index(index_name)?;
    let label = format!(
        "table `{}` index `{}`",
        short_name(table.name()),
        short_name(index_name)
    );
    // The same structural walk as a primary tree. The cursor below only ever sees the
    // entries a scan can reach, and reads them the way a query does, so on its own it
    // cannot tell a tree that is missing pages from a tree that is smaller.
    verify_tree(
        index.tree(),
        ValueKind::Reference(table.tree(), overflow),
        &label,
        options,
        report,
        report_charge,
        store,
        failure,
    )?;
    if failure.is_some() {
        return Ok(());
    }
    let reporter = &mut Reporter {
        options,
        label: &label,
        report,
        charge: report_charge,
        store,
        failure,
    };
    if reporter.full() {
        return Ok(());
    }
    // Walking the index through its public cursor also resolves every PageRef, which
    // is exactly the part that can dangle.
    let mut cursor = index.scan(crate::query::Order::Ascending)?;
    let mut count = 0u64;
    loop {
        match cursor.advance() {
            Ok(true) => {
                count += 1;
                reporter.report.entries_checked += 1;
                if reporter.full() {
                    return Ok(());
                }
            }
            Ok(false) => break,
            Err(e) => {
                reporter.failed(e.location().page, e);
                break;
            }
        }
    }
    // A count that could not be taken is not a count that agreed: swallowing the failure
    // and comparing the scan against itself says the two paths match when neither ran.
    match index.count() {
        Ok(counted) if counted != count => reporter.problem(
            None,
            format!("count_range says {counted} but the scan produced {count}"),
        ),
        Ok(_) => {}
        Err(e) => reporter.failed(e.location().page, e),
    }
    Ok(())
}

/// Longest problem message the report keeps.
const MAX_PROBLEM_MESSAGE: usize = 512;

/// Characters of a table or index name a label carries.
///
/// A name is as long as the file says it is, and it goes in front of every message this
/// pass produces. Building the whole thing and cutting it afterwards spends the memory
/// first, which is what the reservation is supposed to prevent.
const MAX_NAME_IN_LABEL: usize = 64;

/// Renders a name for a label, bounded.
fn short_name(name: &str) -> String {
    let mut cut = MAX_NAME_IN_LABEL.min(name.len());
    while !name.is_char_boundary(cut) {
        cut -= 1;
    }
    if cut == name.len() {
        return name.to_string();
    }
    format!("{}... ({} bytes)", &name[..cut], name.len())
}

/// What one problem costs beyond its message: the struct, the vector slot it goes in, an
/// allocation header, and the room those take while the list is grown and the message is
/// built.
const PROBLEM_OVERHEAD: u64 = 256;

/// Where a walk records what it finds.
///
/// Bundles the three things every check needs, so the checks take what they are checking
/// and not a list of bookkeeping arguments.
struct Reporter<'a> {
    options: &'a VerifyOptions,
    /// Names the tree in every message this walk produces.
    label: &'a str,
    report: &'a mut VerifyReport,
    /// What the report has cost so far.
    ///
    /// The report grows while the pass runs, so it is reserved as it grows and not left
    /// to `max_problems` alone: the caller's limit bounds how many problems there are,
    /// but a hundred of them is tens of kilobytes, which a small budget does not have.
    /// The pass stops recording rather than growing past the budget. Once it returns, the
    /// report is the caller's value, like anything else copied out of a database.
    charge: &'a mut Charge,
    /// Where the room for it comes from.
    ///
    /// Through the store rather than the budget directly, so a report can displace
    /// cached pages: the report is the answer the pass exists to give, and the pages are
    /// a convenience it can rebuild by reading them again.
    store: &'a Arc<PageStore>,
    /// The first thing that stopped the pass rather than being wrong with the file.
    ///
    /// Running out of budget is not a finding: the pass did not learn that the page is
    /// damaged, it learned that it could not look. That is the caller's to fix, so it
    /// comes back as an error instead of going in the report.
    failure: &'a mut Option<Error>,
}

impl Reporter<'_> {
    /// Whether something stopped the pass rather than describing the file.
    ///
    /// Running out of budget and failing to read are both of that kind: the pass did not
    /// learn that a page is damaged, it learned that it could not look. Which of the two
    /// it was is the caller's to fix, and neither is a finding about the file.
    fn stops_the_pass(e: &Error) -> bool {
        matches!(e.kind(), ErrorKind::BudgetExceeded | ErrorKind::Io)
    }

    /// Records what went wrong, unless what went wrong stops the pass.
    fn failed(&mut self, page: Option<u64>, e: Error) {
        if Reporter::stops_the_pass(&e) {
            if self.failure.is_none() {
                *self.failure = Some(e);
            }
            return;
        }
        self.problem(page, e.to_string());
    }

    /// The same, for a message that says where in the page the failure was.
    fn failed_with(&mut self, page: Option<u64>, where_: String, e: Error) {
        if Reporter::stops_the_pass(&e) {
            if self.failure.is_none() {
                *self.failure = Some(e);
            }
            return;
        }
        self.problem(page, format!("{where_}: {e}"));
    }

    /// Whether the pass has hit something that stops it.
    fn stopped(&self) -> bool {
        self.failure.is_some()
    }

    /// Records a problem, keeping the message shape uniform and the limit honoured.
    ///
    /// The limit is enforced here rather than at each caller: one page fails several
    /// different checks, and a caller that only looks at the limit between pages, or
    /// between entries, overshoots by however many checks it ran in between.
    fn problem(&mut self, page: Option<u64>, message: impl AsRef<str>) {
        if self.full() {
            self.report.truncated = true;
            return;
        }
        // Reserved before the message is built, at the most one can cost, because what a
        // message will come to is only known once it exists. Over-reserving by the
        // difference is the price of never allocating first.
        match self
            .store
            .reserve(MAX_PROBLEM_MESSAGE as u64 + PROBLEM_OVERHEAD)
        {
            Ok(more) => self.charge.absorb(more),
            Err(_) => {
                // No room to remember it, even after reclaiming. Saying so is better
                // than growing past the budget the caller set.
                self.report.truncated = true;
                return;
            }
        }
        let full = format!("{}: {}", self.label, message.as_ref());
        // Rebuilt rather than truncated: `String::truncate` keeps the capacity, so the
        // bound would hold for what the message says and not for what it costs.
        let message = if full.len() > MAX_PROBLEM_MESSAGE {
            let mut cut = MAX_PROBLEM_MESSAGE;
            while !full.is_char_boundary(cut) {
                cut -= 1;
            }
            let mut short = String::with_capacity(cut + 3);
            short.push_str(&full[..cut]);
            short.push_str("...");
            short
        } else {
            full
        };
        // Grown by one rather than doubled, so the list never holds room it is not using.
        self.report.problems.reserve_exact(1);
        self.report.problems.push(VerifyProblem { page, message });
    }

    /// Whether the report has reached the caller's limit, or the pass has stopped.
    fn full(&self) -> bool {
        self.stopped() || self.report.problems.len() >= self.options.max_problems
    }
}

/// Loads a page for the walk, reporting a failure rather than stopping the pass.
fn walk_page(tree: &Tree, ordinal: PageOrdinal, reporter: &mut Reporter<'_>) -> Option<PagePin> {
    match tree.raw_page(ordinal) {
        Ok(pin) => Some(pin),
        Err(e) => {
            reporter.failed(Some(ordinal.get()), e);
            None
        }
    }
}

/// Checks the digests of one page against its keys, with the rule searches use.
///
/// Run here whatever `OpenOptions::validate_digests` says, because a verification pass
/// is the place that is supposed to find this.
fn check_page_digests(
    tree: &Tree,
    view: &PageView<'_>,
    ordinal: PageOrdinal,
    reporter: &mut Reporter<'_>,
) {
    if let Err(e) = tree.check_digests(view) {
        reporter.failed(Some(ordinal.get()), e);
    }
}

/// Steps along one tree level's sibling chain in lockstep with the children the level
/// above declares.
///
/// A level's pages and its parents' child references are two descriptions of the same
/// sequence, so comparing them one at a time needs only the page being advanced over: it
/// catches a chain that stops early, a chain with pages no parent points at, and a child
/// reference that names a real page other than the one that belongs there. The chain's
/// start is taken from the first child, because that is where the walk of that level will
/// begin as well.
struct LowerChain {
    current: Option<PageOrdinal>,
    started: bool,
    steps_left: u64,
    broken: bool,
}

impl LowerChain {
    fn new(steps_left: u64) -> LowerChain {
        LowerChain {
            current: None,
            started: false,
            steps_left,
            broken: false,
        }
    }

    /// Matches one declared child against the chain and advances.
    ///
    /// `separator` is what the parent says this child starts at, which is checked against
    /// what the child actually starts with.
    fn expect(
        &mut self,
        tree: &Tree,
        child: PageOrdinal,
        parent: PageOrdinal,
        separator: Separator<'_>,
        reporter: &mut Reporter<'_>,
    ) {
        if self.broken {
            return;
        }
        if !self.started {
            self.started = true;
            self.current = Some(child);
        }
        match self.current {
            Some(current) if current == child => {}
            Some(current) => {
                reporter.problem(
                    Some(parent.get()),
                    format!(
                        "this page's child is page {} but the level below continues at page {}",
                        child.get(),
                        current.get()
                    ),
                );
                self.broken = true;
                return;
            }
            None => {
                reporter.problem(
                    Some(parent.get()),
                    format!(
                        "this page points at page {} but the level below has no more pages",
                        child.get()
                    ),
                );
                self.broken = true;
                return;
            }
        }
        if self.steps_left == 0 {
            reporter.problem(
                Some(child.get()),
                "the sibling chain visits more pages than the file contains",
            );
            self.broken = true;
            return;
        }
        self.steps_left -= 1;
        self.current = match tree.raw_page(child) {
            Ok(pin) => match pin.view() {
                Ok(view) => {
                    check_separator(tree, &view, separator, child, parent, reporter);
                    view.right_sibling()
                }
                Err(e) => {
                    reporter.failed(Some(child.get()), e);
                    self.broken = true;
                    None
                }
            },
            Err(e) => {
                reporter.failed(Some(child.get()), e);
                self.broken = true;
                None
            }
        };
    }

    /// Reports pages the level below has that no parent pointed at.
    fn finish(&self, reporter: &mut Reporter<'_>) {
        if self.broken {
            return;
        }
        if let Some(extra) = self.current {
            reporter.problem(
                Some(extra.get()),
                "no page of the level above points at this page or the ones after it",
            );
        }
    }
}

/// What a parent says one of its children starts at.
#[derive(Clone, Copy)]
enum Separator<'a> {
    /// The separator's key bytes, from a page that stores them.
    Key(&'a [u8]),
    /// The separator's digest, from a page that does not.
    Digest(u64),
}

/// Checks that a child starts where its parent says it does.
///
/// Every other check can pass while this one fails: the page numbers line up, the sibling
/// links line up, and each page is ordered correctly, but a separator that no longer
/// matches its child sends a descent into the wrong page, and a key that is really there
/// is reported as absent.
fn check_separator(
    tree: &Tree,
    child_view: &PageView<'_>,
    separator: Separator<'_>,
    child: PageOrdinal,
    parent: PageOrdinal,
    reporter: &mut Reporter<'_>,
) {
    if child_view.entry_count() == 0 {
        // Nothing to compare, and nothing that should be here: an internal page only
        // points at pages that hold entries, and a page that lost its own would drop
        // every row on it out of every scan without any other check noticing.
        reporter.problem(
            Some(child.get()),
            format!(
                "page {} has no entries but page {parent} points at it",
                child.get()
            ),
        );
        return;
    }
    let wrong = |reporter: &mut Reporter<'_>, detail: String| {
        reporter.problem(
            Some(parent.get()),
            format!("the separator for page {} {detail}", child.get()),
        );
    };
    // The child's first key when it has one, its first digest when it does not. Both
    // builders pick one layout for a whole tree, so a separator with key bytes over a
    // child that omits its own does not come from either of them; it is still compared,
    // through the digest, rather than passed over.
    let child_key = if child_view.layout().omitted_keys {
        None
    } else {
        let first = match child_view.kind() {
            NodeKind::Leaf => child_view
                .leaf_entry(0)
                .and_then(|e| child_view.leaf_key(&e)),
            NodeKind::Internal => child_view
                .internal_entry(0)
                .and_then(|e| child_view.internal_key(&e)),
        };
        match first {
            Ok(key) => Some(key),
            Err(e) => {
                reporter.failed(Some(child.get()), e);
                return;
            }
        }
    };
    let child_digest = match first_digest(child_view) {
        Ok(digest) => digest,
        Err(e) => {
            reporter.failed(Some(child.get()), e);
            return;
        }
    };

    match separator {
        Separator::Key(key) => match child_key {
            Some(first) if first != key => wrong(
                reporter,
                format!(
                    "is {} but that page starts at {}",
                    crate::encoding::describe_key(tree.encoding().as_ref(), key),
                    crate::encoding::describe_key(tree.encoding().as_ref(), first)
                ),
            ),
            Some(_) => {}
            None => match tree.encoding().accepts_digest(key, child_digest) {
                Ok(true) => {}
                Ok(false) => wrong(
                    reporter,
                    format!(
                        "is {} but that page starts at digest {child_digest:#018x}",
                        crate::encoding::describe_key(tree.encoding().as_ref(), key)
                    ),
                ),
                Err(e) => reporter.failed(Some(parent.get()), e),
            },
        },
        Separator::Digest(digest) => {
            if digest != child_digest {
                wrong(
                    reporter,
                    format!(
                        "digests to {digest:#018x} but that page starts at \
                         {child_digest:#018x}"
                    ),
                );
            }
        }
    }
}

/// A key copied out of a page, with the reservation that paid for it.
///
/// One type rather than two bindings, because a struct drops its fields in the order
/// they are declared while a pair of locals drops in the reverse: the reservation has to
/// go after the bytes it stands for, or the budget reports room that is still in use.
struct ChargedKey {
    key: Vec<u8>,
    _charge: Charge,
}

/// The key of one entry on a page of the primary tree, rebuilt when the page omits it.
///
/// The reservation comes back with the key, because the caller keeps the key and looks
/// it up, which reads more pages while it is still held.
fn target_key(
    primary: &Tree,
    view: &PageView<'_>,
    index: usize,
) -> crate::error::Result<ChargedKey> {
    let entry = view.leaf_entry(index)?;
    if !view.layout().omitted_keys {
        let key = view.leaf_key(&entry)?;
        let charge = primary
            .store()
            .reserve(key.len() as u64 + crate::budget::BUFFER_OVERHEAD)?;
        return Ok(ChargedKey {
            key: key.to_vec(),
            _charge: charge,
        });
    }
    let capacity = primary.rebuilt_key_capacity()?;
    let charge = primary
        .store()
        .reserve(capacity as u64 + crate::budget::BUFFER_OVERHEAD)?;
    let mut buf = Vec::with_capacity(capacity);
    primary.rebuild_key(view, index, &mut buf)?;
    Ok(ChargedKey {
        key: buf,
        _charge: charge,
    })
}

/// The digest of a page's first entry, whichever way the digests are laid out.
fn first_digest(view: &PageView<'_>) -> crate::error::Result<u64> {
    if !view.layout().eytzinger {
        return view.digest_at(0);
    }
    // In-order position zero is the leftmost node of the complete tree.
    let complete = view.digest_slot_count();
    let mut node = 1usize;
    while node * 2 <= complete {
        node *= 2;
    }
    view.digest_slot(node - 1)
}

/// Walks every internal level of the tree and returns where the leaf chain starts.
///
/// Levels are walked through their sibling chains, so a page that no search on this file
/// happens to descend into is still read and checked. Each level is also matched against
/// the children the level above declares, which is what finds a chain cut short or a
/// child reference that points at the wrong page.
///
/// Returns `None` when the tree is empty or the walk could not reach the leaf level.
fn verify_internal_levels(
    tree: &Tree,
    previous_key: &mut Vec<u8>,
    previous_charge: &mut Charge,
    reporter: &mut Reporter<'_>,
) -> Result<Option<PageOrdinal>> {
    let Some(mut level_start) = tree.root() else {
        return Ok(None);
    };
    let page_count = tree.store().page_count();
    let mut levels_left = tree.store().limits().max_tree_depth;
    // A step budget rather than a set of visited pages: a cycle is caught just as well,
    // and the check then costs the same on a file of any size.
    let mut steps_left = page_count.saturating_add(1);
    let mut is_root = true;

    loop {
        if reporter.full() {
            return Ok(None);
        }
        // One page is enough to tell whether this level is the leaf level.
        let Some(pin) = walk_page(tree, level_start, reporter) else {
            return Ok(None);
        };
        let is_leaf = match pin.view() {
            Ok(view) => {
                if is_root {
                    // A search starts at the root and can only go down and to the right
                    // of where it lands, so a root with a page beside it is a tree whose
                    // other pages no search reaches. Every other check passes on such a
                    // file: the levels below it are intact, they are just not all under
                    // this root.
                    if view.left_sibling().is_some() || view.right_sibling().is_some() {
                        reporter.problem(
                            Some(level_start.get()),
                            "the root has a sibling, so part of the tree is below no root",
                        );
                    }
                    is_root = false;
                }
                view.kind() == NodeKind::Leaf
            }
            Err(e) => {
                reporter.failed(Some(level_start.get()), e);
                return Ok(None);
            }
        };
        drop(pin);
        // Counted for the leaf level too, because a search counts every page it reads on
        // the way down. A tree that this pass called sound but a search cannot descend is
        // worse than either answer on its own.
        if levels_left == 0 {
            reporter.problem(
                Some(level_start.get()),
                "the tree is deeper than the level limit; a search cannot reach its leaves",
            );
            return Ok(None);
        }
        levels_left -= 1;
        if is_leaf {
            return Ok(Some(level_start));
        }

        let mut first_child: Option<PageOrdinal> = None;
        let mut lower = LowerChain::new(page_count.saturating_add(1));
        let mut current = Some(level_start);
        let mut expected_left: Option<PageOrdinal> = None;
        // One separator is kept to compare the next one against, and a separator is a
        // caller's key, so the copy is reserved before it grows.
        previous_key.clear();
        let mut has_previous = false;

        while let Some(ordinal) = current {
            if reporter.full() {
                return Ok(None);
            }
            if steps_left == 0 {
                reporter.problem(
                    Some(ordinal.get()),
                    "the internal sibling chain visits more pages than the file contains",
                );
                return Ok(None);
            }
            steps_left -= 1;

            let Some(pin) = walk_page(tree, ordinal, reporter) else {
                return Ok(None);
            };
            let view = match pin.view() {
                Ok(v) => v,
                Err(e) => {
                    reporter.failed(Some(ordinal.get()), e);
                    return Ok(None);
                }
            };
            if view.kind() != NodeKind::Internal {
                reporter.problem(Some(ordinal.get()), "an internal level reaches a leaf page");
                return Ok(None);
            }
            if view.entry_count() == 0 {
                reporter.problem(Some(ordinal.get()), "internal page has no children");
                return Ok(None);
            }
            if view.left_sibling() != expected_left {
                reporter.problem(
                    Some(ordinal.get()),
                    format!(
                        "left sibling is {:?} but the previous page was {expected_left:?}",
                        view.left_sibling()
                    ),
                );
            }
            check_page_digests(tree, &view, ordinal, reporter);

            for i in 0..view.entry_count() {
                if reporter.full() {
                    return Ok(None);
                }
                let entry = match view.internal_entry(i) {
                    Ok(entry) => entry,
                    Err(e) => {
                        reporter.failed_with(Some(ordinal.get()), format!("entry {i}"), e);
                        continue;
                    }
                };
                if first_child.is_none() {
                    first_child = Some(entry.child);
                }
                // A child that is not a page of this file turns a search into an error
                // rather than a wrong answer, so it belongs in the report even though
                // the walk never loads it here.
                if entry.child.get() >= page_count {
                    reporter.problem(
                        Some(ordinal.get()),
                        format!(
                            "child {i} is page {} but the file has {page_count} pages",
                            entry.child.get()
                        ),
                    );
                } else if entry.child == ordinal {
                    reporter.problem(
                        Some(ordinal.get()),
                        format!("child {i} points at this page"),
                    );
                } else {
                    let separator = if view.layout().omitted_keys {
                        match view.digest_at(i) {
                            Ok(digest) => Some(Separator::Digest(digest)),
                            Err(e) => {
                                reporter.failed_with(Some(ordinal.get()), format!("entry {i}"), e);
                                None
                            }
                        }
                    } else {
                        match view.internal_key(&entry) {
                            Ok(key) => Some(Separator::Key(key)),
                            Err(e) => {
                                reporter.failed_with(Some(ordinal.get()), format!("entry {i}"), e);
                                None
                            }
                        }
                    };
                    if let Some(separator) = separator {
                        lower.expect(tree, entry.child, ordinal, separator, reporter);
                    }
                }
                if view.layout().omitted_keys {
                    continue;
                }
                let key = match view.internal_key(&entry) {
                    Ok(key) => key,
                    Err(e) => {
                        reporter.failed_with(Some(ordinal.get()), format!("entry {i}"), e);
                        continue;
                    }
                };
                if has_previous {
                    match tree.encoding().compare(previous_key, key) {
                        Ok(std::cmp::Ordering::Less) => {}
                        Ok(_) => reporter.problem(
                            Some(ordinal.get()),
                            format!("separator {i} does not sort above the previous one"),
                        ),
                        Err(e) => {
                            reporter.failed_with(Some(ordinal.get()), format!("separator {i}"), e)
                        }
                    }
                }
                crate::query::charge_buffer(tree, previous_charge, previous_key, key.len())?;
                previous_key.clear();
                previous_key.extend_from_slice(key);
                has_previous = true;
            }

            expected_left = Some(ordinal);
            current = view.right_sibling();
        }

        lower.finish(reporter);

        let Some(next) = first_child else {
            reporter.problem(Some(level_start.get()), "no child to descend into");
            return Ok(None);
        };
        level_start = next;
    }
}

/// Checks that an overflow value points at a blob page of this file.
///
/// The leaf holds only the page number, so a reference that survives the structural
/// checks can still name a page that is not a blob, or no page at all. A search that
/// reaches it fails, which makes it exactly the kind of thing this pass is for.
fn check_overflow_value(
    tree: &Tree,
    page: PageOrdinal,
    entry: usize,
    ordinal: PageOrdinal,
    reporter: &mut Reporter<'_>,
) {
    let page_count = tree.store().page_count();
    if page.get() >= page_count {
        reporter.problem(
            Some(ordinal.get()),
            format!(
                "entry {entry} stores its value on page {} but the file has {page_count} pages",
                page.get()
            ),
        );
        return;
    }
    let pin = match tree.raw_page(page) {
        Ok(pin) => pin,
        Err(e) => {
            reporter.failed_with(
                Some(ordinal.get()),
                format!("entry {entry}: value page {}", page.get()),
                e,
            );
            return;
        }
    };
    match pin.view() {
        Ok(view) if view.kind() == NodeKind::Leaf && view.entry_count() == 0 => {}
        Ok(_) => reporter.problem(
            Some(ordinal.get()),
            format!(
                "entry {entry} stores its value on page {}, which is a tree page rather than \
                 a blob page",
                page.get()
            ),
        ),
        Err(e) => reporter.failed_with(
            Some(ordinal.get()),
            format!("entry {entry}: value page {}", page.get()),
            e,
        ),
    }
}

/// The pages a primary tree keeps its overflowed values on.
///
/// A blob page says nothing about which tree it belongs to, so an index reference into
/// one can only be tied back to its table by remembering which pages that table used. A
/// bit per page of the file, reserved like everything else: a pass that cannot afford it
/// says so rather than skipping the check and calling the file sound.
struct OverflowPages {
    bits: Vec<u64>,
    _charge: Charge,
}

impl OverflowPages {
    fn new(store: &Arc<PageStore>) -> Result<OverflowPages> {
        let words = (store.page_count() as usize).div_ceil(64);
        let charge =
            store.reserve((words as u64).saturating_mul(8) + crate::budget::BUFFER_OVERHEAD)?;
        Ok(OverflowPages {
            bits: vec![0u64; words],
            _charge: charge,
        })
    }

    fn insert(&mut self, page: PageOrdinal) {
        let index = page.get() as usize;
        if let Some(word) = self.bits.get_mut(index / 64) {
            *word |= 1 << (index % 64);
        }
    }

    fn contains(&self, page: PageOrdinal) -> bool {
        let index = page.get() as usize;
        self.bits
            .get(index / 64)
            .is_some_and(|word| word & (1 << (index % 64)) != 0)
    }
}

/// What a tree's leaves hold in the value position.
enum ValueKind<'a> {
    /// The record bytes, and nothing needs to know where the overflowed ones live.
    Plain,
    /// The record bytes, with the pages the overflowed ones live on recorded.
    Record(&'a mut OverflowPages),
    /// A [`PageRef`] into the given primary tree.
    Reference(&'a Tree, &'a OverflowPages),
}

/// Checks that a secondary index entry points at a value that is really there.
///
/// The read path only checks that the range fits in the page and does not start in its
/// header, because finding the record it belongs to costs a scan of the target page.
/// Here that scan is affordable, and it is what tells a reference that lands on a record
/// from one that lands on the metadata between records and hands those bytes back as a
/// value.
/// `primary` is the tree the reference is supposed to point into. A reference that lands
/// on a value of some other tree, its own index included, is as wrong as one that lands
/// between records: the bytes it hands back are not a record.
#[allow(clippy::too_many_arguments)]
fn check_page_ref(
    tree: &Tree,
    primary: &Tree,
    overflow: &OverflowPages,
    value: &[u8],
    entry: usize,
    ordinal: PageOrdinal,
    reporter: &mut Reporter<'_>,
) {
    let reference = match PageRef::parse(value) {
        Ok(reference) => reference,
        Err(e) => {
            reporter.failed_with(Some(ordinal.get()), format!("entry {entry}"), e);
            return;
        }
    };
    let Some(pin) = walk_page(tree, reference.page, reporter) else {
        return;
    };
    let range = match reference.range(pin.len()) {
        Ok(range) => range,
        Err(e) => {
            reporter.failed_with(Some(ordinal.get()), format!("entry {entry}"), e);
            return;
        }
    };
    let view = match pin.view() {
        Ok(view) => view,
        Err(e) => {
            reporter.failed(Some(reference.page.get()), e);
            return;
        }
    };
    if view.entry_count() == 0 {
        // A blob page: the value is the whole payload, and it has to be one of this
        // table's. Nothing on the page says whose it is, so the primary walk recorded
        // them.
        if !overflow.contains(reference.page) {
            reporter.problem(
                Some(ordinal.get()),
                format!(
                    "entry {entry} points at page {}, which holds no value of this table",
                    reference.page.get()
                ),
            );
            return;
        }
        let payload = crate::format::PAGE_PREFIX_LEN..pin.len();
        if range != payload {
            reporter.problem(
                Some(ordinal.get()),
                format!(
                    "entry {entry} points at {}..{} of blob page {}, which holds {}..{}",
                    range.start,
                    range.end,
                    reference.page.get(),
                    payload.start,
                    payload.end
                ),
            );
        }
        return;
    }
    for i in 0..view.entry_count() {
        let target = match view.leaf_entry(i) {
            Ok(target) => target,
            Err(e) => {
                reporter.failed_with(Some(reference.page.get()), format!("entry {i}"), e);
                return;
            }
        };
        match target.value {
            LeafValue::Inline { offset, len } if offset == range.start && len == range.len() => {}
            _ => continue,
        }
        // The range is some entry's value. Whose, though: looking the key up in the
        // primary tree says whether this page belongs to it and whether the record it
        // would find is the one the reference names.
        let target = match target_key(primary, &view, i) {
            Ok(charged) => charged,
            Err(e) => {
                reporter.failed_with(Some(reference.page.get()), format!("entry {i}"), e);
                return;
            }
        };
        match primary.find(&target.key) {
            Ok(Some((found, at))) if found.ordinal() == reference.page => {
                match found.view().and_then(|v| v.leaf_entry(at)) {
                    Ok(record) => match record.value {
                        LeafValue::Inline { offset, len }
                            if offset == range.start && len == range.len() =>
                        {
                            return
                        }
                        _ => {}
                    },
                    Err(e) => {
                        reporter.failed(Some(reference.page.get()), e);
                        return;
                    }
                }
            }
            Ok(_) => {}
            Err(e) => {
                reporter.failed_with(Some(ordinal.get()), format!("entry {entry}"), e);
                return;
            }
        }
        break;
    }
    reporter.problem(
        Some(ordinal.get()),
        format!(
            "entry {entry} points at {}..{} of page {}, which is not the value of any \
             record on it",
            range.start,
            range.end,
            reference.page.get()
        ),
    );
}

#[allow(clippy::too_many_arguments)]
fn verify_tree(
    tree: &Tree,
    mut values: ValueKind<'_>,
    label: &str,
    options: &VerifyOptions,
    report: &mut VerifyReport,
    report_charge: &mut Charge,
    report_store: &Arc<PageStore>,
    failure: &mut Option<Error>,
) -> Result<()> {
    let reporter = &mut Reporter {
        options,
        label,
        report,
        charge: report_charge,
        store: report_store,
        failure,
    };
    reporter.report.trees_checked += 1;
    let budget = tree.store().budget();
    // The reservation is declared before the buffer it stands for, so it is dropped
    // after it: a charge released while the bytes are still there leaves the budget
    // reporting room that is not free.
    let mut previous_charge = budget.try_reserve(0)?;
    let mut previous_key = Vec::new();
    let Some(leftmost) =
        verify_internal_levels(tree, &mut previous_key, &mut previous_charge, reporter)?
    else {
        return Ok(());
    };
    // A step budget rather than a set of visited pages: a cycle is caught just as well,
    // and the check then costs the same on a file of any size.
    let mut steps_left = tree.store().page_count().saturating_add(1);
    // The walk holds two keys: the one it rebuilt, and a copy of the one before it. Both
    // are a caller's keys, of a caller's length, so both are reserved before they grow.
    let mut key_charge = budget.try_reserve(0)?;
    let mut key_buf = Vec::new();
    previous_key.clear();
    let mut has_previous = false;
    let mut previous_rebuilt = false;

    let Some(mut pin) = walk_page(tree, leftmost, reporter) else {
        return Ok(());
    };
    let mut expected_left: Option<PageOrdinal> = None;

    loop {
        if reporter.full() {
            return Ok(());
        }
        let ordinal = pin.ordinal();
        if steps_left == 0 {
            reporter.problem(
                Some(ordinal.get()),
                "the leaf sibling chain visits more pages than the file contains",
            );
            return Ok(());
        }
        steps_left -= 1;
        let view = match pin.view() {
            Ok(v) => v,
            Err(e) => {
                reporter.failed(Some(ordinal.get()), e);
                return Ok(());
            }
        };
        if view.kind() != NodeKind::Leaf {
            reporter.problem(
                Some(ordinal.get()),
                "the leaf chain reaches an internal page",
            );
            return Ok(());
        }
        if view.left_sibling() != expected_left {
            reporter.problem(
                Some(ordinal.get()),
                format!(
                    "left sibling is {:?} but the previous page was {expected_left:?}",
                    view.left_sibling()
                ),
            );
        }
        check_page_digests(tree, &view, ordinal, reporter);

        for i in 0..view.entry_count() {
            if reporter.full() {
                return Ok(());
            }
            let entry = match view.leaf_entry(i) {
                Ok(entry) => entry,
                Err(e) => {
                    reporter.failed_with(Some(ordinal.get()), format!("entry {i}"), e);
                    continue;
                }
            };
            match entry.value {
                LeafValue::Overflow { page } => {
                    check_overflow_value(tree, page, i, ordinal, reporter);
                    if let ValueKind::Record(ref mut overflow) = values {
                        overflow.insert(page);
                    }
                    if let ValueKind::Reference(primary, overflow) = &values {
                        // The reference itself did not fit on the page, so read it back
                        // from where it went and check it like any other.
                        if let Some(blob) = walk_page(tree, page, reporter) {
                            // Borrowed from the page, never copied: a reference is
                            // sixteen bytes, but a page that a damaged entry points at
                            // is as large as the file says it is.
                            match blob.view() {
                                Ok(blob_view) => check_page_ref(
                                    tree,
                                    primary,
                                    overflow,
                                    blob_view.blob_payload(),
                                    i,
                                    ordinal,
                                    reporter,
                                ),
                                Err(e) => {
                                    reporter.failed_with(Some(page.get()), format!("entry {i}"), e)
                                }
                            }
                        }
                    }
                }
                LeafValue::Inline { offset, len } => {
                    if let ValueKind::Reference(primary, overflow) = &values {
                        match view.inline_value(offset, len) {
                            Ok(value) => {
                                check_page_ref(tree, primary, overflow, value, i, ordinal, reporter)
                            }
                            Err(e) => {
                                reporter.failed_with(Some(ordinal.get()), format!("entry {i}"), e)
                            }
                        }
                    }
                }
            }
            let key: &[u8] = if view.layout().omitted_keys {
                let capacity = match tree.rebuilt_key_capacity() {
                    Ok(capacity) => capacity,
                    Err(e) => {
                        reporter.failed_with(Some(ordinal.get()), format!("entry {i}"), e);
                        return Ok(());
                    }
                };
                crate::query::charge_buffer(tree, &mut key_charge, &mut key_buf, capacity)?;
                match tree.rebuild_key(&view, i, &mut key_buf) {
                    Ok(true) => {
                        crate::query::charge_written(tree, &mut key_charge, &mut key_buf)?;
                        &key_buf
                    }
                    Ok(false) | Err(_) => {
                        reporter.problem(
                            Some(ordinal.get()),
                            format!("entry {i}: the key could not be rebuilt"),
                        );
                        continue;
                    }
                }
            } else {
                match view.leaf_key(&entry) {
                    Ok(k) => k,
                    Err(e) => {
                        reporter.failed_with(Some(ordinal.get()), format!("entry {i}"), e);
                        continue;
                    }
                }
            };
            // Every entry is compared with the one before it, across pages as well as
            // within one, because a tree can change layout at a page boundary and then
            // neither side's own rule sees the crossing.
            //
            // A key rebuilt from a digest under an encoding whose digest is not injective
            // is not the key the page was written with: the C# builder writes non-unique
            // index pages that way and the record id is simply not there, so a run of
            // them all rebuild to the same thing. Equal is allowed when either side is
            // such a key, and strictly ascending otherwise.
            let rebuilt = tree.separators_are_inexact(&view);
            if has_previous {
                let ordering = tree.encoding().compare(&previous_key, key);
                let wrong = match ordering {
                    Ok(std::cmp::Ordering::Less) => false,
                    Ok(std::cmp::Ordering::Equal) => !(rebuilt || previous_rebuilt),
                    Ok(std::cmp::Ordering::Greater) => true,
                    Err(e) => {
                        reporter.failed_with(Some(ordinal.get()), format!("entry {i}"), e);
                        continue;
                    }
                };
                if wrong {
                    reporter.problem(
                        Some(ordinal.get()),
                        format!("entry {i} does not sort above the previous entry"),
                    );
                }
            }
            crate::query::charge_buffer(tree, &mut previous_charge, &mut previous_key, key.len())?;
            previous_key.clear();
            previous_key.extend_from_slice(key);
            previous_rebuilt = rebuilt;
            has_previous = true;
            reporter.report.entries_checked += 1;
        }

        let Some(right) = view.right_sibling() else {
            return Ok(());
        };
        expected_left = Some(ordinal);
        let Some(next) = walk_page(tree, right, reporter) else {
            return Ok(());
        };
        pin = next;
    }
}
