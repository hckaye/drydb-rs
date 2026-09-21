//! Which rkyv configuration a stored archive was written with.
//!
//! rkyv's endianness, pointer width and alignment are Cargo features, and Cargo features
//! are additive across a whole dependency graph. Another crate turning on `big_endian`
//! silently changes the bytes this one writes. That cannot be prevented, so it is
//! detected: the configuration in force is measured at run time, recorded in every
//! envelope, and checked on read.

use std::sync::OnceLock;

/// Byte order of archived integers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endian {
    /// Least significant byte first.
    Little,
    /// Most significant byte first.
    Big,
}

/// The rkyv configuration an archive was written with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Profile {
    /// rkyv's archived pointer width, in bytes.
    pub pointer_width: u8,
    /// Byte order of archived integers.
    pub endian: Endian,
    /// Whether archived primitives carry their natural alignment.
    pub aligned: bool,
    /// rkyv's minor series, e.g. 8 for the 0.8 line.
    pub series: u8,
}

/// The compact form of a [`Profile`] stored in an envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProfileId(pub u16);

impl std::fmt::Display for ProfileId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#06x}", self.0)
    }
}

/// The rkyv series this crate is built against.
pub const SERIES: u8 = 8;

impl Profile {
    /// Measures the rkyv configuration this binary was built with.
    pub fn current() -> Profile {
        static CURRENT: OnceLock<Profile> = OnceLock::new();
        *CURRENT.get_or_init(detect)
    }

    /// The compact identifier stored in envelopes.
    pub fn id(self) -> ProfileId {
        let width = match self.pointer_width {
            2 => 0u16,
            4 => 1,
            8 => 2,
            _ => 3,
        };
        let endian = if self.endian == Endian::Big {
            1u16 << 2
        } else {
            0
        };
        let aligned = if self.aligned { 0u16 } else { 1 << 3 };
        ProfileId(((self.series as u16) << 8) | width | endian | aligned)
    }

    /// A human-readable description, for error messages.
    pub fn describe(self) -> String {
        format!(
            "rkyv 0.{}, {}-endian, {}-byte pointers, {}",
            self.series,
            if self.endian == Endian::Little {
                "little"
            } else {
                "big"
            },
            self.pointer_width,
            if self.aligned { "aligned" } else { "unaligned" }
        )
    }
}

/// Whether `T` is the same type as `U`.
fn is<T: 'static, U: 'static>() -> bool {
    core::any::TypeId::of::<T>() == core::any::TypeId::of::<U>()
}

fn detect() -> Profile {
    // The archived pointer width is a type size, so it can be read directly.
    let pointer_width = core::mem::size_of::<rkyv::primitive::ArchivedUsize>() as u8;
    // Alignment likewise: the `unaligned` feature drops it to one.
    let aligned = core::mem::align_of::<rkyv::primitive::ArchivedU32>() > 1;
    // Byte order is fixed when rkyv is built, and the archived integer is whichever of
    // rend's types that choice names, so asking which one it is settles it. Archiving a
    // known value and looking at the bytes would settle it too, but that allocates a
    // serializer's arena, and this runs on the first read of a value rather than
    // anywhere a caller could have reserved for it.
    let endian = if is::<rkyv::primitive::ArchivedU32, rkyv::rend::u32_be>()
        || is::<rkyv::primitive::ArchivedU32, rkyv::rend::unaligned::u32_ube>()
    {
        Endian::Big
    } else if is::<rkyv::primitive::ArchivedU32, rkyv::rend::u32_le>()
        || is::<rkyv::primitive::ArchivedU32, rkyv::rend::unaligned::u32_ule>()
    {
        Endian::Little
    } else {
        // Built to keep integers in the order the machine uses, so that is the order.
        if cfg!(target_endian = "little") {
            Endian::Little
        } else {
            Endian::Big
        }
    };
    Profile {
        pointer_width,
        endian,
        aligned,
        series: SERIES,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_profile_id_round_trips_its_parts() {
        let profile = Profile {
            pointer_width: 4,
            endian: Endian::Little,
            aligned: true,
            series: 8,
        };
        assert_eq!(profile.id(), ProfileId(0x0801));

        let other = Profile {
            endian: Endian::Big,
            aligned: false,
            ..profile
        };
        assert_ne!(other.id(), profile.id());
    }

    #[test]
    fn the_detected_profile_is_the_documented_default() {
        // If this fails, some crate in the build graph turned on one of rkyv's format
        // features, and archives written by this build are not the ones the README
        // describes.
        let profile = Profile::current();
        assert_eq!(profile.series, 8);
        assert_eq!(
            profile.pointer_width,
            4,
            "expected rkyv's default 32-bit archived pointers, got {}",
            profile.describe()
        );
        assert_eq!(profile.endian, Endian::Little, "{}", profile.describe());
        assert!(profile.aligned, "{}", profile.describe());
    }
}
