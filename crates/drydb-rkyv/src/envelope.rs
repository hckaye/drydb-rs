//! The wrapper that identifies a stored archive.
//!
//! DryDB 1.4 has no field to say what a value contains, and this crate does not add one:
//! the description travels inside the value, and the core reader never looks at it. Only
//! code that has explicitly opened a table as typed reads an envelope, so a raw value
//! that happens to start with the same four bytes is never mistaken for an archive.
//!
//! ```text
//! 0  4  magic "DRYR"
//! 4  2  envelope version (u16)
//! 6  2  codec profile id (u16)
//! 8  8  schema id (u64)
//! 16 2  schema version (u16)
//! 18 2  flags (u16, must be zero)
//! 20 4  archive length (u32)
//! 24 .. archive bytes
//! ```
//!
//! Every field is little-endian, and every one is read with an explicit bounds check.

use drydb::{Error, ErrorKind, Result};

use crate::profile::ProfileId;

/// Magic bytes at the start of an envelope.
pub const MAGIC: [u8; 4] = *b"DRYR";
/// Size of the envelope header.
pub const HEADER_LEN: usize = 24;
/// The only envelope version this build writes and reads.
pub const VERSION: u16 = 1;

/// An application-assigned identifier for a value's schema.
///
/// It is persisted, so it has to be stable across builds: do not derive it from a Rust
/// type name or `TypeId`, both of which change without the format changing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SchemaId(pub u64);

impl std::fmt::Display for SchemaId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#018x}", self.0)
    }
}

/// What a type declares about how it is stored.
pub trait RkyvSchema {
    /// Stable identifier for this schema.
    const SCHEMA_ID: SchemaId;
    /// Version of this schema. Values are never migrated implicitly; a reader that wants
    /// an older version has to ask for it.
    const SCHEMA_VERSION: u16;
}

/// A decoded envelope header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    /// Envelope format version.
    pub version: u16,
    /// Codec profile the archive was written with.
    pub profile: ProfileId,
    /// Schema identifier.
    pub schema: SchemaId,
    /// Schema version.
    pub schema_version: u16,
    /// Byte range of the archive inside the value.
    pub archive: std::ops::Range<usize>,
}

impl Envelope {
    /// Parses and validates an envelope at the start of `value`.
    pub fn parse(value: &[u8]) -> Result<Envelope> {
        if value.len() < HEADER_LEN {
            return Err(Error::new(
                ErrorKind::SchemaMismatch,
                format!(
                    "value is {} bytes, shorter than the {HEADER_LEN} byte envelope",
                    value.len()
                ),
            ));
        }
        if value[0..4] != MAGIC {
            return Err(Error::new(
                ErrorKind::SchemaMismatch,
                "value does not start with an rkyv envelope",
            ));
        }
        let version = u16::from_le_bytes([value[4], value[5]]);
        if version != VERSION {
            return Err(Error::new(
                ErrorKind::SchemaMismatch,
                format!("envelope version {version} is not supported; this build reads {VERSION}"),
            ));
        }
        let profile = ProfileId(u16::from_le_bytes([value[6], value[7]]));
        let schema = SchemaId(u64::from_le_bytes(
            value[8..16].try_into().expect("checked length"),
        ));
        let schema_version = u16::from_le_bytes([value[16], value[17]]);
        let flags = u16::from_le_bytes([value[18], value[19]]);
        if flags != 0 {
            return Err(Error::new(
                ErrorKind::SchemaMismatch,
                format!("envelope sets unknown flags {flags:#06x}"),
            ));
        }
        let length = u32::from_le_bytes(value[20..24].try_into().expect("checked length")) as usize;
        let end = HEADER_LEN.checked_add(length).ok_or_else(|| {
            Error::new(
                ErrorKind::SchemaMismatch,
                "envelope archive length overflows",
            )
        })?;
        if end > value.len() {
            return Err(Error::new(
                ErrorKind::SchemaMismatch,
                format!(
                    "envelope claims a {length} byte archive but only {} bytes follow it",
                    value.len() - HEADER_LEN
                ),
            ));
        }
        if end != value.len() {
            return Err(Error::new(
                ErrorKind::SchemaMismatch,
                format!(
                    "value has {} trailing bytes after the archive",
                    value.len() - end
                ),
            ));
        }
        Ok(Envelope {
            version,
            profile,
            schema,
            schema_version,
            archive: HEADER_LEN..end,
        })
    }

    /// Writes an envelope header for an archive of `archive_len` bytes.
    pub fn encode(
        profile: ProfileId,
        schema: SchemaId,
        schema_version: u16,
        archive_len: usize,
    ) -> Result<[u8; HEADER_LEN]> {
        let length = u32::try_from(archive_len).map_err(|_| {
            Error::new(
                ErrorKind::ValueTooLarge,
                "an archive longer than 4 GiB cannot be described by this envelope",
            )
        })?;
        let mut out = [0u8; HEADER_LEN];
        out[0..4].copy_from_slice(&MAGIC);
        out[4..6].copy_from_slice(&VERSION.to_le_bytes());
        out[6..8].copy_from_slice(&profile.0.to_le_bytes());
        out[8..16].copy_from_slice(&schema.0.to_le_bytes());
        out[16..18].copy_from_slice(&schema_version.to_le_bytes());
        out[18..20].copy_from_slice(&0u16.to_le_bytes());
        out[20..24].copy_from_slice(&length.to_le_bytes());
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(archive: &[u8]) -> Vec<u8> {
        let mut out = Envelope::encode(ProfileId(0x0123), SchemaId(0xDEAD_BEEF), 7, archive.len())
            .unwrap()
            .to_vec();
        out.extend_from_slice(archive);
        out
    }

    #[test]
    fn round_trips() {
        let bytes = sample(b"0123456789abcdef");
        let envelope = Envelope::parse(&bytes).unwrap();
        assert_eq!(envelope.version, VERSION);
        assert_eq!(envelope.profile, ProfileId(0x0123));
        assert_eq!(envelope.schema, SchemaId(0xDEAD_BEEF));
        assert_eq!(envelope.schema_version, 7);
        assert_eq!(&bytes[envelope.archive], b"0123456789abcdef");
    }

    #[test]
    fn golden_bytes_are_stable() {
        // The envelope layout is persisted, so this is a format test, not a round trip.
        let bytes = sample(b"AB");
        assert_eq!(
            bytes,
            vec![
                b'D', b'R', b'Y', b'R', // magic
                0x01, 0x00, // version
                0x23, 0x01, // profile
                0xEF, 0xBE, 0xAD, 0xDE, 0x00, 0x00, 0x00, 0x00, // schema id
                0x07, 0x00, // schema version
                0x00, 0x00, // flags
                0x02, 0x00, 0x00, 0x00, // archive length
                b'A', b'B',
            ]
        );
    }

    #[test]
    fn rejects_foreign_values() {
        assert!(Envelope::parse(b"not an envelope at all....").is_err());
        assert!(Envelope::parse(b"").is_err());
        assert!(Envelope::parse(b"DRYR").is_err());
    }

    #[test]
    fn rejects_every_truncation() {
        let bytes = sample(b"0123456789abcdef");
        for len in 0..bytes.len() {
            assert!(
                Envelope::parse(&bytes[..len]).is_err(),
                "prefix of {len} bytes parsed"
            );
        }
    }

    #[test]
    fn rejects_trailing_bytes() {
        let mut bytes = sample(b"AB");
        bytes.push(0);
        assert!(Envelope::parse(&bytes).is_err());
    }

    #[test]
    fn rejects_unknown_versions_and_flags() {
        let mut bytes = sample(b"AB");
        bytes[4] = 9;
        assert!(Envelope::parse(&bytes).is_err());

        let mut bytes = sample(b"AB");
        bytes[18] = 1;
        assert!(Envelope::parse(&bytes).is_err());
    }
}
