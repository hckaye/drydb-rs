//! A record-level [rkyv](https://rkyv.org) value codec for [`drydb`].
//!
//! One DryDB value holds one self-contained rkyv archive, wrapped in a small envelope
//! that says which schema and which rkyv configuration produced it. The database's own
//! structures -- header, catalog, page directory, B+Tree nodes -- stay exactly as the
//! 1.4 format defines them, so a file written this way is still an ordinary DryDB file
//! and the C# implementation still reads these values as opaque bytes.
//!
//! ```no_run
//! use std::sync::Arc;
//! use drydb::{Database, Int64Encoding};
//! use drydb_rkyv::{Codec, RkyvSchema, SchemaId};
//!
//! #[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
//! struct Monster { name: String, hp: u32 }
//!
//! impl RkyvSchema for Monster {
//!     const SCHEMA_ID: SchemaId = SchemaId(0x4D4F_4E53_5445_5201);
//!     const SCHEMA_VERSION: u16 = 1;
//! }
//!
//! let codec = Codec::new();
//! let db = Database::open("game.drydb")?;
//! let table = db.table("monsters")?;
//!
//! if let Some(value) = table.get(&1i64.to_le_bytes())? {
//!     let prepared = codec.prepare::<Monster>(value)?;
//!     let monster = prepared.access()?;
//!     println!("{} has {} hp", monster.name, monster.hp);
//! }
//! # Ok::<(), drydb::Error>(())
//! ```
//!
//! # What "zero copy" means here
//!
//! | Situation | What happens |
//! | --- | --- |
//! | Cached page, archive already aligned | The archive is borrowed from the page. Nothing is copied and nothing is deserialised. |
//! | Cache miss | The page is read from the file, as any query would. Typed access adds no copy on top. |
//! | Filtered (compressed) page | The page is decoded into a buffer first. If the archive lands aligned, typed access still borrows. |
//! | Archive not aligned | That one value is copied into an aligned buffer, which is charged to the memory budget. |
//! | [`Prepared::deserialize`] | A full rkyv deserialise, with the allocations the owned type needs. |
//! | [`Prepared::access`] on a schema with `Arc` or `Rc` | Validating remembers every shared pointer it follows. Charged only if [`Codec::validation_headroom`] says how much room to give it. |
//!
//! A value's byte offset inside its page is decided by the builder's packing, so whether
//! an archive lands aligned is not something either side controls. The copy path is
//! therefore normal, not exceptional; [`Prepared::was_copied`] says which one ran, and
//! [`Codec::allow_copy`] turns the copy into an error for callers that would rather know.
//!
//! # Trust boundary
//!
//! Every access validates the archive with rkyv's checked API against *that value's
//! byte range only*, never the surrounding page. A relative pointer that leaves the
//! archive is rejected even when it lands somewhere else in the same page.

#![deny(unsafe_code)]
#![warn(missing_docs)]
#![warn(missing_debug_implementations)]
#![warn(rust_2018_idioms)]

mod envelope;
mod profile;

use std::marker::PhantomData;
use std::sync::Arc;

use drydb::{Budget, Charge, Error, ErrorKind, Reserve, Result, ValueGuard, BUFFER_OVERHEAD};
use rkyv::api::high::{HighSerializer, HighValidator};
use rkyv::bytecheck::CheckBytes;
use rkyv::rancor;
use rkyv::ser::allocator::ArenaHandle;
use rkyv::util::AlignedVec;

/// Room validation is given by default, as a multiple of the archive it checks.
///
/// Zero, because what validating costs depends on the schema and not on the bytes in
/// front of it. It costs nothing at all unless the archive holds shared pointers, since
/// what it spends is what it takes to remember each one it has already followed; for an
/// archive that is almost nothing but shared pointers it was measured at ten and a half
/// times its length, and one packing them as densely as they go would reach about thirty
/// times. Reserving that for every value would refuse reads that a budget could easily
/// have done, and rkyv offers no way to cap what a validation spends, so the caller says
/// whether their schema needs the room. See [`Codec::validation_headroom`].
const DEFAULT_VALIDATION_HEADROOM: u64 = 0;
use rkyv::{Archive, Portable, Serialize};

pub use envelope::{Envelope, RkyvSchema, SchemaId, HEADER_LEN, MAGIC, VERSION};
pub use profile::{Endian, Profile, ProfileId, SERIES};

/// Alignment of the buffer a misaligned archive is copied into.
///
/// Archived types needing more than this are refused rather than accessed through a
/// buffer that does not satisfy them.
pub const COPY_ALIGNMENT: usize = 64;

/// Alignment an archive's buffer must have before it can be borrowed in place.
///
/// The root type's own `align_of` is *not* the answer: an archive of a type whose
/// archived form aligns to 4 can still contain a `u64` or a `u128`, and rkyv places each
/// object at its natural alignment measured from the start of the buffer. Borrowing
/// therefore needs the buffer to satisfy the strongest alignment anything inside it
/// might want, which for rkyv's 128-bit primitives is 16 bytes. A schema with a custom
/// archived type that wants more says so through [`Codec::require_alignment`].
pub const DEFAULT_ALIGNMENT: usize = 16;

/// The serializer type an archivable value has to satisfy.
pub type CodecSerializer<'a> = HighSerializer<AlignedVec, ArenaHandle<'a>, rancor::Error>;

/// Limits and policy for typed access.
#[derive(Debug, Clone)]
pub struct Codec {
    max_value_bytes: usize,
    allow_copy: bool,
    alignment: usize,
    reserver: Option<Arc<dyn Reserve>>,
    validation_headroom: u64,
}

impl Default for Codec {
    fn default() -> Self {
        Codec {
            max_value_bytes: 64 << 20,
            allow_copy: true,
            alignment: DEFAULT_ALIGNMENT,
            reserver: None,
            validation_headroom: DEFAULT_VALIDATION_HEADROOM,
        }
    }
}

impl Codec {
    /// A codec with default limits.
    pub fn new() -> Codec {
        Codec::default()
    }

    /// Largest value this codec will serialize or prepare.
    pub fn max_value_bytes(mut self, bytes: usize) -> Self {
        self.max_value_bytes = bytes;
        self
    }

    /// Whether a misaligned archive may be copied into an aligned buffer.
    ///
    /// With this off, a misaligned archive reports
    /// [`ErrorKind::Unsupported`] instead, which is how a caller that wants to know it
    /// is never copying finds out.
    pub fn allow_copy(mut self, allow: bool) -> Self {
        self.allow_copy = allow;
        self
    }

    /// Charges aligned copies to a memory budget, normally the one the database uses.
    ///
    /// A budget on its own has nothing to reclaim from, so a copy that would fit once
    /// the cache gave a page back is refused. [`Codec::reserve_with`] takes the database
    /// itself, which reclaims.
    pub fn budget(mut self, budget: Arc<Budget>) -> Self {
        self.reserver = Some(Arc::new(budget) as Arc<dyn Reserve>);
        self
    }

    /// Charges what this codec allocates to a database, which reclaims to make room.
    ///
    /// Both [`Database`](drydb::Database) and [`Table`](drydb::Table) can be handed in.
    pub fn reserve_with(mut self, reserver: Arc<dyn Reserve>) -> Self {
        self.reserver = Some(reserver);
        self
    }

    /// How much room validation is given, as a multiple of the archive it checks.
    ///
    /// Zero by default, which reserves nothing: a schema without shared pointers spends
    /// nothing on validation, and reserving for one that might would refuse reads a
    /// budget could easily have done. A schema that does use `Arc` or `Rc` should set
    /// this, because what validating then spends is what it takes to remember every
    /// shared pointer it has followed, and that is memory the budget otherwise never
    /// hears about. Ten and a half times the archive covered the densest case measured
    /// here; thirty covers one packing them as tightly as they go.
    pub fn validation_headroom(mut self, multiple: u64) -> Self {
        self.validation_headroom = multiple;
        self
    }

    /// Raises the alignment a borrowed archive must have.
    ///
    /// Only needed for a schema whose archived form contains a custom type wanting more
    /// than [`DEFAULT_ALIGNMENT`]. Must be a power of two no larger than
    /// [`COPY_ALIGNMENT`].
    pub fn require_alignment(mut self, bytes: usize) -> Result<Self> {
        if !bytes.is_power_of_two() || bytes > COPY_ALIGNMENT {
            return Err(Error::new(
                ErrorKind::InvalidArgument,
                format!(
                    "alignment must be a power of two no larger than {COPY_ALIGNMENT}, got {bytes}"
                ),
            ));
        }
        self.alignment = bytes;
        Ok(self)
    }

    /// The alignment a borrowed archive of `T` must have.
    pub fn alignment_for<T: Archive>(&self) -> usize {
        core::mem::align_of::<T::Archived>().max(self.alignment)
    }

    /// The rkyv configuration this build writes.
    pub fn profile(&self) -> Profile {
        Profile::current()
    }

    /// Serializes one value into an enveloped DryDB value.
    ///
    /// Each call builds its own archive: no serializer state, shared pointer table or
    /// arena carries over between records, so an archive never points outside itself.
    pub fn serialize<T>(&self, value: &T) -> Result<Vec<u8>>
    where
        T: RkyvSchema + for<'a> Serialize<CodecSerializer<'a>>,
    {
        let archive = rkyv::to_bytes::<rancor::Error>(value).map_err(|e| {
            Error::new(
                ErrorKind::ArchiveValidation,
                format!("rkyv serialization failed: {e}"),
            )
        })?;
        let total = HEADER_LEN.checked_add(archive.len()).ok_or_else(|| {
            Error::new(ErrorKind::ValueTooLarge, "serialized value size overflows")
        })?;
        if total > self.max_value_bytes {
            return Err(Error::new(
                ErrorKind::ValueTooLarge,
                format!(
                    "serialized value is {total} bytes, above the {} byte limit",
                    self.max_value_bytes
                ),
            ));
        }
        let header = Envelope::encode(
            Profile::current().id(),
            T::SCHEMA_ID,
            T::SCHEMA_VERSION,
            archive.len(),
        )?;
        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(&header);
        out.extend_from_slice(archive.as_ref());
        Ok(out)
    }

    /// Reads the envelope of a value without touching the archive.
    pub fn inspect(&self, value: &[u8]) -> Result<Envelope> {
        Envelope::parse(value)
    }

    /// Prepares a value held by a [`ValueGuard`] for typed access.
    ///
    /// Borrows the page when the archive is aligned for `T`, and copies that one value
    /// into an aligned buffer when it is not.
    pub fn prepare<T>(&self, guard: ValueGuard) -> Result<Prepared<T>>
    where
        T: Archive + RkyvSchema,
    {
        let envelope = self.check_envelope::<T>(guard.as_bytes())?;
        let range = envelope.archive.clone();
        let alignment = self.alignment_for::<T>();
        self.check_alignment_supported(alignment)?;

        let address = guard.as_bytes()[range.start..].as_ptr() as usize;
        if address % alignment == 0 {
            return Ok(Prepared {
                source: Source::Borrowed { guard, range },
                envelope,
                reserver: self.reserver.clone(),
                validation_headroom: self.validation_headroom,
                marker: PhantomData,
            });
        }
        if !self.allow_copy {
            return Err(Error::new(
                ErrorKind::Unsupported,
                format!(
                    "this value's archive sits at an address that is not {alignment}-byte \
                     aligned, and copying is disabled"
                ),
            ));
        }
        let buffer = self.copy_aligned(&guard.as_bytes()[range])?;
        Ok(Prepared {
            source: Source::Owned(buffer),
            envelope,
            reserver: self.reserver.clone(),
            validation_headroom: self.validation_headroom,
            marker: PhantomData,
        })
    }

    /// Prepares a value from plain bytes, always copying into an aligned buffer.
    ///
    /// For values that did not come from a page, such as one just serialized.
    pub fn prepare_bytes<T>(&self, value: &[u8]) -> Result<Prepared<T>>
    where
        T: Archive + RkyvSchema,
    {
        let envelope = self.check_envelope::<T>(value)?;
        self.check_alignment_supported(self.alignment_for::<T>())?;
        let buffer = self.copy_aligned(&value[envelope.archive.clone()])?;
        Ok(Prepared {
            source: Source::Owned(buffer),
            envelope,
            reserver: self.reserver.clone(),
            validation_headroom: self.validation_headroom,
            marker: PhantomData,
        })
    }

    fn check_alignment_supported(&self, alignment: usize) -> Result<()> {
        if alignment > COPY_ALIGNMENT {
            return Err(Error::new(
                ErrorKind::Unsupported,
                format!(
                    "this archived type needs {alignment}-byte alignment, above the \
                     {COPY_ALIGNMENT}-byte buffers this codec can provide"
                ),
            ));
        }
        Ok(())
    }

    fn check_envelope<T: RkyvSchema>(&self, value: &[u8]) -> Result<Envelope> {
        if value.len() > self.max_value_bytes {
            return Err(Error::new(
                ErrorKind::ValueTooLarge,
                format!(
                    "value is {} bytes, above the {} byte limit",
                    value.len(),
                    self.max_value_bytes
                ),
            ));
        }
        let envelope = Envelope::parse(value)?;
        let expected = Profile::current();
        if envelope.profile != expected.id() {
            return Err(Error::new(
                ErrorKind::SchemaMismatch,
                format!(
                    "value was written with codec profile {} but this build is {} ({})",
                    envelope.profile,
                    expected.id(),
                    expected.describe()
                ),
            ));
        }
        if envelope.schema != T::SCHEMA_ID {
            return Err(Error::new(
                ErrorKind::SchemaMismatch,
                format!(
                    "value holds schema {} but {} was asked for",
                    envelope.schema,
                    T::SCHEMA_ID
                ),
            ));
        }
        if envelope.schema_version != T::SCHEMA_VERSION {
            return Err(Error::new(
                ErrorKind::SchemaMismatch,
                format!(
                    "value holds schema {} version {} but version {} was asked for; \
                     values are never migrated implicitly",
                    envelope.schema,
                    envelope.schema_version,
                    T::SCHEMA_VERSION
                ),
            ));
        }
        Ok(envelope)
    }

    fn copy_aligned(&self, archive: &[u8]) -> Result<OwnedArchive> {
        let charge = match &self.reserver {
            Some(reserver) => Some(reserver.reserve(archive.len() as u64 + BUFFER_OVERHEAD)?),
            None => None,
        };
        let mut buffer = AlignedVec::<COPY_ALIGNMENT>::with_capacity(archive.len());
        buffer.extend_from_slice(archive);
        Ok(OwnedArchive {
            buffer,
            _charge: charge,
        })
    }
}

#[derive(Debug)]
struct OwnedArchive {
    buffer: AlignedVec<COPY_ALIGNMENT>,
    /// Held only for its `Drop`, which returns the bytes to the budget.
    _charge: Option<Charge>,
}

#[derive(Debug)]
enum Source {
    Borrowed {
        guard: ValueGuard,
        range: std::ops::Range<usize>,
    },
    Owned(OwnedArchive),
}

/// A value ready for typed access.
///
/// Holds whatever keeps the archive's bytes alive: either the page the value lives on,
/// or the aligned copy that was made of it. References handed out by
/// [`Prepared::access`] borrow from this, so they cannot outlive it.
#[derive(Debug)]
pub struct Prepared<T> {
    source: Source,
    envelope: Envelope,
    /// Where the room for validating this archive comes from, if anywhere.
    reserver: Option<Arc<dyn Reserve>>,
    /// How much of it to ask for, as a multiple of the archive.
    validation_headroom: u64,
    marker: PhantomData<fn() -> T>,
}

impl<T> Prepared<T> {
    /// The archive's bytes.
    pub fn archive_bytes(&self) -> &[u8] {
        match &self.source {
            Source::Borrowed { guard, range } => &guard.as_bytes()[range.clone()],
            Source::Owned(owned) => owned.buffer.as_slice(),
        }
    }

    /// The envelope this value carried.
    pub fn envelope(&self) -> &Envelope {
        &self.envelope
    }

    /// Whether the archive had to be copied into an aligned buffer.
    pub fn was_copied(&self) -> bool {
        matches!(self.source, Source::Owned(_))
    }

    /// The page the value lives on, when it was borrowed rather than copied.
    pub fn page(&self) -> Option<drydb::PageOrdinal> {
        match &self.source {
            Source::Borrowed { guard, .. } => Some(guard.page()),
            Source::Owned(_) => None,
        }
    }
}

impl<T> Prepared<T>
where
    T: Archive,
    T::Archived: Portable + for<'a> CheckBytes<HighValidator<'a, rancor::Error>>,
{
    /// Validates the archive and borrows it as `&Archived<T>`.
    ///
    /// Validation runs on every call, over this value's bytes alone. One returned
    /// reference can then be used to read as many fields as you like.
    pub fn access(&self) -> Result<&T::Archived> {
        // Validation walks the archive remembering every shared pointer it has followed,
        // which is memory this crate spends on the caller's behalf like any other. Set
        // aside before it runs and given back after, so a budget that cannot hold it
        // says so rather than being quietly gone past.
        let wanted = (self.archive_bytes().len() as u64).saturating_mul(self.validation_headroom);
        let _headroom = match (&self.reserver, wanted) {
            (Some(reserver), 1..) => Some(reserver.reserve(wanted)?),
            _ => None,
        };
        rkyv::access::<T::Archived, rancor::Error>(self.archive_bytes()).map_err(|e| {
            Error::new(
                ErrorKind::ArchiveValidation,
                format!("archive failed validation: {e}"),
            )
        })
    }
}

impl<T> Prepared<T>
where
    T: Archive,
    T::Archived: Portable
        + for<'a> CheckBytes<HighValidator<'a, rancor::Error>>
        + rkyv::Deserialize<T, rkyv::api::high::HighDeserializer<rancor::Error>>,
{
    /// Validates the archive and deserialises it into an owned `T`.
    ///
    /// This is the opposite of borrowing: it allocates whatever the owned type needs.
    pub fn deserialize(&self) -> Result<T> {
        let archived = self.access()?;
        rkyv::deserialize::<T, rancor::Error>(archived).map_err(|e| {
            Error::new(
                ErrorKind::ArchiveValidation,
                format!("rkyv deserialization failed: {e}"),
            )
        })
    }
}
