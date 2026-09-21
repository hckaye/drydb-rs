//! Arbitrary bytes as an enveloped rkyv value.
//!
//! The interesting inputs are the ones whose envelope parses, because those reach rkyv's
//! validator with a byte range this crate chose.

#![no_main]

use drydb_rkyv::{Codec, RkyvSchema, SchemaId};
use libfuzzer_sys::fuzz_target;

/// Covers the shapes whose validation has something to check: a string (UTF-8), a
/// vector of a type wider than the archived pointer, an enum discriminant, an option,
/// and a nested struct.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
struct Fixture {
    name: String,
    values: Vec<u64>,
    flag: bool,
    kind: Kind,
    inner: Option<Inner>,
}

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
enum Kind {
    Empty,
    Tagged(u32),
    Named { label: String },
}

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
struct Inner {
    id: u128,
    parts: Vec<String>,
}

impl RkyvSchema for Fixture {
    const SCHEMA_ID: SchemaId = SchemaId(0x465A_5A5F_0000_0001);
    const SCHEMA_VERSION: u16 = 1;
}

fuzz_target!(|data: &[u8]| {
    let codec = Codec::new().max_value_bytes(1 << 20);

    // Raw input: most of it is rejected at the envelope.
    if let Ok(prepared) = codec.prepare_bytes::<Fixture>(data) {
        if let Ok(archived) = prepared.access() {
            let _ = archived.name.len() + archived.values.len() + usize::from(archived.flag);
        }
    }

    // Input wrapped in a well-formed envelope, so the archive itself is what is tested.
    if data.len() > 4 {
        let mut value = Vec::with_capacity(drydb_rkyv::HEADER_LEN + data.len());
        let header = drydb_rkyv::Envelope::encode(
            drydb_rkyv::Profile::current().id(),
            Fixture::SCHEMA_ID,
            Fixture::SCHEMA_VERSION,
            data.len(),
        )
        .expect("the length fits");
        value.extend_from_slice(&header);
        value.extend_from_slice(data);
        if let Ok(prepared) = codec.prepare_bytes::<Fixture>(&value) {
            if let Ok(archived) = prepared.access() {
                let _ = archived.name.len() + archived.values.len();
                let _ = prepared.deserialize();
            }
        }
    }
});
