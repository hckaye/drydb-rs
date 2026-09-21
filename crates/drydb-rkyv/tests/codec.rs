//! Typed access: envelopes, profiles, alignment, and what a damaged archive does.

use std::sync::Arc;

use drydb::{Budget, Database, DatabaseBuilder, ErrorKind, Int64Encoding, Order};
use drydb_rkyv::{Codec, Profile, RkyvSchema, SchemaId};

#[derive(Debug, PartialEq, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
#[rkyv(derive(Debug))]
struct Monster {
    name: String,
    hp: u32,
    drops: Vec<u64>,
}

impl RkyvSchema for Monster {
    const SCHEMA_ID: SchemaId = SchemaId(0x4D4F_4E53_5445_5201);
    const SCHEMA_VERSION: u16 = 1;
}

#[derive(Debug, PartialEq, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
struct Loot {
    id: u64,
}

impl RkyvSchema for Loot {
    const SCHEMA_ID: SchemaId = SchemaId(0x4C4F_4F54_0000_0001);
    const SCHEMA_VERSION: u16 = 3;
}

fn monster(i: u32) -> Monster {
    Monster {
        name: format!("monster-{i}{}", "x".repeat((i % 17) as usize)),
        hp: i.wrapping_mul(7),
        drops: (0..(i % 5) as u64).collect(),
    }
}

#[test]
fn serialize_and_access_round_trips() {
    let codec = Codec::new();
    for i in 0..32 {
        let value = monster(i);
        let bytes = codec.serialize(&value).unwrap();
        let prepared = codec.prepare_bytes::<Monster>(&bytes).unwrap();
        let archived = prepared.access().unwrap();
        assert_eq!(archived.name, value.name);
        assert_eq!(archived.hp, value.hp);
        assert_eq!(archived.drops.len(), value.drops.len());
        assert_eq!(prepared.deserialize().unwrap(), value);
    }
}

#[test]
fn the_envelope_describes_the_value() {
    let codec = Codec::new();
    let bytes = codec.serialize(&monster(1)).unwrap();
    let envelope = codec.inspect(&bytes).unwrap();
    assert_eq!(envelope.schema, Monster::SCHEMA_ID);
    assert_eq!(envelope.schema_version, Monster::SCHEMA_VERSION);
    assert_eq!(envelope.profile, Profile::current().id());
    assert_eq!(envelope.archive.start, drydb_rkyv::HEADER_LEN);
    assert_eq!(envelope.archive.end, bytes.len());
}

#[test]
fn a_different_schema_is_refused() {
    let codec = Codec::new();
    let bytes = codec.serialize(&monster(1)).unwrap();
    let err = codec.prepare_bytes::<Loot>(&bytes).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::SchemaMismatch);
}

#[test]
fn a_different_schema_version_is_refused() {
    let codec = Codec::new();
    let mut bytes = codec.serialize(&monster(1)).unwrap();
    bytes[16..18].copy_from_slice(&99u16.to_le_bytes());
    let err = codec.prepare_bytes::<Monster>(&bytes).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::SchemaMismatch);
    assert!(err.to_string().contains("never migrated"), "{err}");
}

#[test]
fn a_different_codec_profile_is_refused() {
    let codec = Codec::new();
    let mut bytes = codec.serialize(&monster(1)).unwrap();
    // Flip the profile's endianness bit, as a build with rkyv's `big_endian` feature
    // would have written.
    let profile = u16::from_le_bytes([bytes[6], bytes[7]]) ^ 0b100;
    bytes[6..8].copy_from_slice(&profile.to_le_bytes());
    let err = codec.prepare_bytes::<Monster>(&bytes).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::SchemaMismatch);
    assert!(err.to_string().contains("codec profile"), "{err}");
}

#[test]
fn a_raw_value_is_not_mistaken_for_an_archive() {
    let codec = Codec::new();
    for raw in [&b""[..], b"hello", b"DRYR", &[0u8; 64][..]] {
        assert!(codec.prepare_bytes::<Monster>(raw).is_err());
    }
}

#[test]
fn a_truncated_value_never_validates() {
    let codec = Codec::new();
    let bytes = codec.serialize(&monster(9)).unwrap();
    for cut in 1..bytes.len() {
        let mut truncated = bytes[..cut].to_vec();
        if truncated.len() > drydb_rkyv::HEADER_LEN {
            // Keep the envelope self-consistent, so the failure has to come from the
            // archive rather than from the length field.
            let length = (truncated.len() - drydb_rkyv::HEADER_LEN) as u32;
            truncated[20..24].copy_from_slice(&length.to_le_bytes());
        }
        if let Ok(prepared) = codec.prepare_bytes::<Monster>(&truncated) {
            assert!(
                prepared.access().is_err(),
                "a {cut} byte prefix validated as a whole archive"
            );
        }
    }
}

#[test]
fn corrupted_archives_fail_validation_rather_than_panicking() {
    let codec = Codec::new();
    let original = codec.serialize(&monster(12)).unwrap();
    let mut state = 0x5EED_1234u64;
    for _ in 0..3000 {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        let offset = drydb_rkyv::HEADER_LEN
            + (state >> 33) as usize % (original.len() - drydb_rkyv::HEADER_LEN);
        let mut bytes = original.clone();
        bytes[offset] ^= ((state >> 11) as u8) | 1;
        // The codec is only read here, so borrowing it across the boundary is sound.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if let Ok(prepared) = codec.prepare_bytes::<Monster>(&bytes) {
                if let Ok(archived) = prepared.access() {
                    // A mutation can land on payload bytes and still leave a valid
                    // archive; reading it must stay inside the value.
                    let _ = archived.name.len() + archived.drops.len();
                }
            }
        }));
        assert!(outcome.is_ok(), "mutating byte {offset} panicked");
    }
}

#[test]
fn a_pointer_that_leaves_the_archive_is_rejected() {
    // Two values, back to back in one buffer. Shrinking the first envelope's archive
    // length leaves its relative pointers aimed past the end of its own range, which is
    // exactly the "points at the neighbouring record" case.
    let codec = Codec::new();
    let first = codec.serialize(&monster(3)).unwrap();
    let second = codec.serialize(&monster(4)).unwrap();

    let mut spliced = first.clone();
    let shortened = (first.len() - drydb_rkyv::HEADER_LEN - 8) as u32;
    spliced[20..24].copy_from_slice(&shortened.to_le_bytes());
    spliced.truncate(drydb_rkyv::HEADER_LEN + shortened as usize);
    spliced.extend_from_slice(&second);

    // The envelope now describes a shorter archive followed by trailing bytes, which is
    // refused outright; the point is that the neighbouring value is never in scope.
    let err = codec.prepare_bytes::<Monster>(&spliced).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::SchemaMismatch);

    // Without the trailing bytes, validation fails on the archive itself.
    let mut shortened_only = first;
    shortened_only[20..24].copy_from_slice(&shortened.to_le_bytes());
    shortened_only.truncate(drydb_rkyv::HEADER_LEN + shortened as usize);
    match codec.prepare_bytes::<Monster>(&shortened_only) {
        Err(_) => {}
        Ok(prepared) => assert!(prepared.access().is_err()),
    }
}

#[test]
fn oversized_values_are_refused() {
    let codec = Codec::new().max_value_bytes(64);
    let err = codec.serialize(&monster(200)).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::ValueTooLarge);
}

fn build_database(count: u32) -> Vec<u8> {
    let codec = Codec::new();
    let mut builder = DatabaseBuilder::new().page_size(4096).unwrap();
    let table = builder
        .create_table("monsters", Arc::new(Int64Encoding))
        .unwrap();
    for i in 0..count {
        let bytes = codec.serialize(&monster(i)).unwrap();
        builder
            .append(table, &Int64Encoding::encode(i as i64), &bytes)
            .unwrap();
    }
    builder.build_to_vec().unwrap()
}

#[test]
fn values_read_back_out_of_a_database() {
    let codec = Codec::new();
    let db = Database::open_bytes(build_database(200)).unwrap();
    let table = db.table("monsters").unwrap();

    let mut copied = 0usize;
    let mut borrowed = 0usize;
    for i in 0..200u32 {
        let guard = table
            .get(&Int64Encoding::encode(i as i64))
            .unwrap()
            .unwrap();

        // The stored bytes are exactly what was serialized, which is what makes them
        // readable as raw bytes from any other implementation.
        assert_eq!(
            guard.as_bytes(),
            codec.serialize(&monster(i)).unwrap().as_slice()
        );

        let archive_address = guard.as_bytes()[drydb_rkyv::HEADER_LEN..].as_ptr() as usize;
        let alignment = codec.alignment_for::<Monster>();
        let expected_copy = archive_address % alignment != 0;

        let prepared = codec.prepare::<Monster>(guard).unwrap();
        assert_eq!(
            prepared.was_copied(),
            expected_copy,
            "value {i}: copying should follow the archive's address alignment"
        );
        if prepared.was_copied() {
            copied += 1;
        } else {
            borrowed += 1;
            assert!(
                prepared.page().is_some(),
                "a borrowed archive knows its page"
            );
        }

        let archived = prepared.access().unwrap();
        let expected = monster(i);
        assert_eq!(archived.name, expected.name);
        assert_eq!(archived.hp, expected.hp);
    }
    // Values are packed back to back, so both paths get exercised by a run of this size.
    assert!(borrowed > 0, "no value was borrowed in place");
    eprintln!("{borrowed} archives borrowed, {copied} copied into aligned buffers");
}

/// The alignment the codec requires has to cover everything inside an archive, not just
/// the root: this schema's archived root aligns to 4 but carries `u64` elements.
#[test]
fn the_required_alignment_covers_inner_objects() {
    let codec = Codec::new();
    assert!(core::mem::align_of::<ArchivedMonster>() <= drydb_rkyv::DEFAULT_ALIGNMENT);
    assert_eq!(
        codec.alignment_for::<Monster>(),
        drydb_rkyv::DEFAULT_ALIGNMENT
    );
    assert!(codec.clone().require_alignment(3).is_err());
    assert!(codec.clone().require_alignment(128).is_err());
    assert!(codec.require_alignment(32).is_ok());
}

#[test]
fn a_scan_can_read_every_value_typed() {
    let codec = Codec::new();
    let db = Database::open_bytes(build_database(150)).unwrap();
    let table = db.table("monsters").unwrap();

    let mut cursor = table.scan(Order::Ascending).unwrap();
    let mut seen = 0u32;
    while cursor.advance().unwrap() {
        let entry = cursor.current().unwrap();
        let guard = entry.to_guard().unwrap();
        let prepared = codec.prepare::<Monster>(guard).unwrap();
        let archived = prepared.access().unwrap();
        assert_eq!(archived.hp, monster(seen).hp);
        seen += 1;
    }
    assert_eq!(seen, 150);
}

#[test]
fn refusing_to_copy_reports_the_alignment() {
    let codec = Codec::new().allow_copy(false);
    let db = Database::open_bytes(build_database(200)).unwrap();
    let table = db.table("monsters").unwrap();

    let mut refused = 0usize;
    for i in 0..200u32 {
        let guard = table
            .get(&Int64Encoding::encode(i as i64))
            .unwrap()
            .unwrap();
        let alignment = codec.alignment_for::<Monster>();
        let aligned = guard.as_bytes()[drydb_rkyv::HEADER_LEN..].as_ptr() as usize % alignment == 0;
        match codec.prepare::<Monster>(guard) {
            Ok(prepared) => {
                assert!(aligned);
                assert!(!prepared.was_copied());
            }
            Err(e) => {
                assert!(!aligned);
                assert_eq!(e.kind(), ErrorKind::Unsupported);
                assert!(e.to_string().contains("aligned"), "{e}");
                refused += 1;
            }
        }
    }
    eprintln!("{refused} of 200 values would have needed a copy");
}

#[test]
fn aligned_copies_are_charged_to_the_budget() {
    let budget = Budget::new(1 << 20);
    let codec = Codec::new().budget(Arc::clone(&budget));
    let bytes = codec.serialize(&monster(5)).unwrap();

    assert_eq!(budget.in_use(), 0);
    let prepared = codec.prepare_bytes::<Monster>(&bytes).unwrap();
    assert!(prepared.was_copied());
    assert!(budget.in_use() > 0, "an aligned copy should be charged");
    drop(prepared);
    assert_eq!(budget.in_use(), 0, "the charge is released with the buffer");
}

#[test]
fn a_copy_that_does_not_fit_the_budget_is_an_error() {
    let budget = Budget::new(64);
    let codec = Codec::new().budget(budget);
    let bytes = codec.serialize(&monster(40)).unwrap();
    let err = codec.prepare_bytes::<Monster>(&bytes).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::BudgetExceeded);
}

#[test]
fn prepared_values_outlive_their_database() {
    let codec = Codec::new();
    let prepared = {
        let db = Database::open_bytes(build_database(20)).unwrap();
        let table = db.table("monsters").unwrap();
        let guard = table.get(&Int64Encoding::encode(7)).unwrap().unwrap();
        codec.prepare::<Monster>(guard).unwrap()
    };
    assert_eq!(prepared.access().unwrap().hp, monster(7).hp);
}

/// The codec charges its copies to a budget, and the one worth charging is the database's
/// own. There used to be no way to reach it, so a reader that copied every value spent
/// memory the database never heard about.
#[test]
fn a_codec_can_charge_the_database_that_supplies_the_values() {
    use drydb::{DatabaseBuilder, Int64Encoding, OpenOptions};

    let record = Monster {
        name: "bookkeeper".to_string(),
        hp: 7,
        drops: vec![0u64; 1_250],
    };
    let mut builder = DatabaseBuilder::new().page_size(512).unwrap();
    let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    builder
        .append(
            table,
            &Int64Encoding::encode(0),
            &Codec::new().serialize(&record).unwrap(),
        )
        .unwrap();
    let db = OpenOptions::new()
        .memory_budget(1 << 20)
        .cache_shards(1)
        .open_bytes(builder.build_to_vec().unwrap())
        .unwrap();
    let table = db.table("t").unwrap();

    // The database's own budget, which is what the copies have to come out of.
    let codec = Codec::new().budget(Arc::clone(db.budget()));
    let before = db.charged_bytes();
    let mut held = Vec::new();
    for _ in 0..8 {
        let guard = table.get(&Int64Encoding::encode(0)).unwrap().unwrap();
        held.push(codec.prepare::<Monster>(guard).unwrap());
    }
    let after = db.charged_bytes();
    assert!(
        after >= before + 8 * 10_000,
        "eight copies of ten thousand bytes went unaccounted: {before} to {after}"
    );

    drop(held);
    let released = after - db.charged_bytes();
    assert!(
        released >= 8 * 10_000,
        "and they are given back: {released} of {after} released"
    );
}

/// Validating an archive that holds shared pointers costs what it takes to remember
/// each one, which used to be spent without asking the budget.
#[test]
fn validating_an_archive_is_charged_to_the_budget() {
    let budget = Budget::new(1 << 20);
    let record = Monster {
        name: "shared".to_string(),
        hp: 1,
        drops: vec![0u64; 2_000],
    };
    let bytes = Codec::new().serialize(&record).unwrap();

    // Asked for, and there is room: the charge is taken and given back.
    let codec = Codec::new()
        .budget(Arc::clone(&budget))
        .validation_headroom(16);
    let prepared = codec.prepare_bytes::<Monster>(&bytes).unwrap();
    let before = budget.in_use();
    assert_eq!(prepared.access().unwrap().drops.len(), 2_000);
    assert_eq!(budget.in_use(), before, "the room is given back");

    // Asked for, and there is not: the read says so rather than spending it anyway.
    let tight = Budget::new(bytes.len() as u64 * 4);
    let codec = Codec::new()
        .budget(Arc::clone(&tight))
        .validation_headroom(16);
    let prepared = codec.prepare_bytes::<Monster>(&bytes).unwrap();
    let err = prepared.access().unwrap_err();
    assert_eq!(err.kind(), ErrorKind::BudgetExceeded);

    // Not asked for, which is the default: a schema without shared pointers spends
    // nothing on validation and is not made to reserve for it.
    let codec = Codec::new().budget(Arc::clone(&tight));
    let prepared = codec.prepare_bytes::<Monster>(&bytes).unwrap();
    assert!(prepared.access().is_ok());
}

/// A budget on its own has nothing to give back, so a copy that would fit once the cache
/// released a page was refused. Handing the codec the database instead lets it reclaim.
#[test]
fn a_codec_given_the_database_reclaims_rather_than_refusing() {
    use drydb::{DatabaseBuilder, Int64Encoding, OpenOptions, Reserve};

    let big = Monster {
        name: "big".to_string(),
        hp: 1,
        drops: vec![0u64; 750],
    };
    let small = Monster {
        name: "small".to_string(),
        hp: 2,
        drops: vec![0u64; 125],
    };
    let mut builder = DatabaseBuilder::new().page_size(512).unwrap();
    let table = builder.create_table("t", Arc::new(Int64Encoding)).unwrap();
    let codec = Codec::new();
    builder
        .append(
            table,
            &Int64Encoding::encode(0),
            &codec.serialize(&big).unwrap(),
        )
        .unwrap();
    for i in 1..30i64 {
        builder
            .append(
                table,
                &Int64Encoding::encode(i),
                &codec.serialize(&small).unwrap(),
            )
            .unwrap();
    }
    let db = Arc::new(
        OpenOptions::new()
            .memory_budget(32_768)
            .cache_capacity(32_768)
            .cache_shards(1)
            .open_bytes(builder.build_to_vec().unwrap())
            .unwrap(),
    );
    let table = db.table("t").unwrap();

    // Held, so the budget stays tight, and the cache fills with the rest.
    let guard = table.get(&Int64Encoding::encode(0)).unwrap().unwrap();
    for i in 1..30i64 {
        let _ = table.get(&Int64Encoding::encode(i));
    }

    let codec = Codec::new().reserve_with(Arc::clone(&db) as Arc<dyn Reserve>);
    let prepared = codec.prepare::<Monster>(guard).unwrap();
    assert_eq!(prepared.access().unwrap().drops.len(), 750);
}
