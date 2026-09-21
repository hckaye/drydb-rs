//! Error type shared by every `drydb` entry point.
//!
//! [`Error`] is cheap to clone (it is a single `Arc`), which lets the page cache hand
//! the same failure to every waiter of a de-duplicated page load without re-running the
//! read. Diagnostics carry page ordinals and byte offsets; they never carry key or
//! value bytes, so an error can be logged without leaking record contents.

use std::fmt;
use std::sync::Arc;

/// Categories of failure returned by this crate.
///
/// A missing key is not an error: lookups return `Option`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorKind {
    /// Underlying file or directory I/O failed.
    Io,
    /// The file is not a DryDB 1.4 container (bad magic, unsupported version).
    UnsupportedFormat,
    /// An index descriptor names a key encoding that is not registered.
    UnknownEncoding,
    /// The header names a page filter that is not registered.
    UnknownFilter,
    /// Structurally invalid on-disk data: out-of-range offsets, inconsistent lengths,
    /// unknown node flags, cyclic or over-deep trees.
    CorruptData,
    /// The managed memory budget cannot satisfy a reservation.
    BudgetExceeded,
    /// A value is larger than the configured or format-imposed limit.
    ValueTooLarge,
    /// A typed codec was asked for a schema that the stored value does not carry.
    SchemaMismatch,
    /// A typed archive failed structural validation.
    ArchiveValidation,
    /// The caller passed arguments that cannot describe a query (reversed bounds,
    /// unknown table or index name, wrong key width for the encoding).
    InvalidArgument,
    /// A valid request that this build cannot serve (disabled feature, streaming a
    /// filtered blob, a layout combination the writer does not emit).
    Unsupported,
}

impl ErrorKind {
    fn as_str(self) -> &'static str {
        match self {
            ErrorKind::Io => "io",
            ErrorKind::UnsupportedFormat => "unsupported format",
            ErrorKind::UnknownEncoding => "unknown key encoding",
            ErrorKind::UnknownFilter => "unknown page filter",
            ErrorKind::CorruptData => "corrupt data",
            ErrorKind::BudgetExceeded => "memory budget exceeded",
            ErrorKind::ValueTooLarge => "value too large",
            ErrorKind::SchemaMismatch => "schema mismatch",
            ErrorKind::ArchiveValidation => "archive validation failed",
            ErrorKind::InvalidArgument => "invalid argument",
            ErrorKind::Unsupported => "unsupported",
        }
    }
}

/// Where a corrupt-data or I/O failure was observed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Location {
    /// Page ordinal, when the failure happened inside a page.
    pub page: Option<u64>,
    /// Byte offset. Relative to the page when `page` is set, otherwise absolute.
    pub offset: Option<u64>,
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.page, self.offset) {
            (Some(p), Some(o)) => write!(f, " (page {p}, offset {o})"),
            (Some(p), None) => write!(f, " (page {p})"),
            (None, Some(o)) => write!(f, " (offset {o})"),
            (None, None) => Ok(()),
        }
    }
}

/// Details attached to [`ErrorKind::BudgetExceeded`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetInfo {
    /// Bytes the failed reservation asked for.
    pub requested: u64,
    /// Bytes charged to the budget at the time of the failure.
    pub in_use: u64,
    /// Configured budget.
    pub limit: u64,
    /// `true` when releasing live guards could let the same request succeed later;
    /// `false` when the request can never fit in the configured budget.
    pub retryable: bool,
}

#[derive(Debug)]
struct Repr {
    kind: ErrorKind,
    message: Box<str>,
    location: Location,
    budget: Option<BudgetInfo>,
    source: Option<std::io::Error>,
}

/// The error type of this crate.
#[derive(Clone)]
pub struct Error(Arc<Repr>);

impl Error {
    /// Builds an error of `kind`.
    ///
    /// Adapter crates that extend this one, such as a value codec, report their
    /// failures through the same type rather than introducing another.
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Error(Arc::new(Repr {
            kind,
            message: message.into().into_boxed_str(),
            location: Location::default(),
            budget: None,
            source: None,
        }))
    }

    pub(crate) fn corrupt(message: impl Into<String>) -> Self {
        Error::new(ErrorKind::CorruptData, message)
    }

    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Error::new(ErrorKind::InvalidArgument, message)
    }

    pub(crate) fn io(message: impl Into<String>, source: std::io::Error) -> Self {
        Error(Arc::new(Repr {
            kind: ErrorKind::Io,
            message: message.into().into_boxed_str(),
            location: Location::default(),
            budget: None,
            source: Some(source),
        }))
    }

    pub(crate) fn budget(info: BudgetInfo) -> Self {
        let message = format!(
            "requested {} bytes, {} of {} in use",
            info.requested, info.in_use, info.limit
        );
        Error(Arc::new(Repr {
            kind: ErrorKind::BudgetExceeded,
            message: message.into_boxed_str(),
            location: Location::default(),
            budget: Some(info),
            source: None,
        }))
    }

    /// Attaches the page ordinal the failure was observed in.
    pub(crate) fn at_page(self, page: u64) -> Self {
        self.map_repr(|r| r.location.page = Some(page))
    }

    /// Attaches a byte offset (relative to the page when one is set).
    pub(crate) fn at_offset(self, offset: u64) -> Self {
        self.map_repr(|r| r.location.offset = Some(offset))
    }

    fn map_repr(self, f: impl FnOnce(&mut Repr)) -> Self {
        let mut repr = match Arc::try_unwrap(self.0) {
            Ok(repr) => repr,
            Err(arc) => Repr {
                kind: arc.kind,
                message: arc.message.clone(),
                location: arc.location,
                budget: arc.budget,
                // `io::Error` is not cloneable; a shared error keeps its message but
                // loses the OS source when it is annotated after sharing.
                source: None,
            },
        };
        f(&mut repr);
        Error(Arc::new(repr))
    }

    /// The failure category.
    pub fn kind(&self) -> ErrorKind {
        self.0.kind
    }

    /// Where the failure was observed, when known.
    pub fn location(&self) -> Location {
        self.0.location
    }

    /// Budget accounting, for [`ErrorKind::BudgetExceeded`].
    pub fn budget_info(&self) -> Option<BudgetInfo> {
        self.0.budget
    }

    /// `true` when retrying after releasing live guards may succeed.
    pub fn is_retryable(&self) -> bool {
        self.0.budget.map(|b| b.retryable).unwrap_or(false)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: {}{}",
            self.0.kind.as_str(),
            self.0.message,
            self.0.location
        )
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Error")
            .field("kind", &self.0.kind)
            .field("message", &self.0.message)
            .field("location", &self.0.location)
            .field("budget", &self.0.budget)
            .field("source", &self.0.source)
            .finish()
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0
            .source
            .as_ref()
            .map(|e| e as &(dyn std::error::Error + 'static))
    }
}

impl From<Error> for std::io::Error {
    fn from(e: Error) -> Self {
        std::io::Error::other(e.to_string())
    }
}

/// Result alias used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;
