//! Key encodings.
//!
//! An encoding fixes three things at once: the byte form of a key, its total order,
//! and an order-preserving 64-bit digest. Format 1.4 stores a digest array on every
//! tree page and searches it first, so the digest is not optional:
//! `digest(a) < digest(b)` must imply `a < b`, and `a < b` must imply
//! `digest(a) <= digest(b)`. Equal digests are ambiguous and fall back to
//! [`KeyEncoding::compare`].
//!
//! When a digest is a bijection over the key space ([`KeyEncoding::is_digest_exact`]),
//! the builder drops the key bytes from the pages entirely and rebuilds them through
//! [`KeyEncoding::decode_key_from_digest`].
//!
//! The built-in ids match the upstream C# registry: `i64`, `ascii`, `uuidv7`, `ulid`.
//! Anything else must be registered on [`OpenOptions`](crate::OpenOptions); unknown ids
//! are rejected rather than guessed.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Arc;

use crate::error::{Error, ErrorKind, Result};

/// Byte form, order and digest of a key column.
///
/// Implementations must be pure: the same input always produces the same order and
/// digest, for the lifetime of any file built with them.
pub trait KeyEncoding: std::fmt::Debug + Send + Sync + 'static {
    /// Registry id stored in the index descriptor.
    fn id(&self) -> &str;

    /// Total order over encoded keys.
    fn compare(&self, a: &[u8], b: &[u8]) -> Result<Ordering>;

    /// Order-preserving 64-bit digest.
    fn digest(&self, key: &[u8]) -> Result<u64>;

    /// Whether [`Self::digest`] is injective, so equal digests imply equal keys.
    fn is_digest_exact(&self) -> bool {
        false
    }

    /// Whether `stored` is an acceptable digest for `key` on a page.
    ///
    /// Normally that means it equals [`Self::digest`]. An encoding overrides this when a
    /// writer it has to interoperate with computes the digest differently but still
    /// order preservingly, so that such a file is read rather than rejected.
    fn accepts_digest(&self, key: &[u8], stored: u64) -> Result<bool> {
        Ok(self.digest(key)? == stored)
    }

    /// Rebuilds the key bytes that a page storing only digests stands for.
    ///
    /// Called for every entry of a page flagged `OmittedKeys`. An encoding that cannot
    /// do this must return an error, which rejects such a page rather than inventing a
    /// key.
    fn decode_key_from_digest(&self, digest: u64, out: &mut Vec<u8>) -> Result<()> {
        let _ = (digest, out);
        Err(Error::new(
            ErrorKind::Unsupported,
            format!("encoding `{}` cannot rebuild keys from digests", self.id()),
        ))
    }

    /// Fixed key width, when the encoding has one. Used to reject malformed keys
    /// early, both from callers and from pages.
    fn fixed_key_len(&self) -> Option<usize> {
        None
    }

    /// Largest buffer [`Self::decode_key_from_digest`] can need, when this encoding
    /// rebuilds keys at all.
    ///
    /// This is what decides whether a page that omits its key bytes can be read. Such a
    /// page is nothing but digests: the search compares them instead of keys and
    /// everything that wants a key rebuilds it from one, so an encoding that cannot
    /// rebuild would answer a lookup for a key that is not there with whatever row the
    /// digest landed on. The buffer also has to be reserved against the memory budget
    /// before the rebuild writes into it, which is why the answer is a size and not a
    /// flag: an encoding whose keys vary in length can still rebuild them, but it cannot
    /// leave the size open.
    ///
    /// The default is `None`, so implementing [`Self::decode_key_from_digest`] without
    /// this leaves those pages refused rather than read on a guess. Being fixed width is
    /// not enough on its own: `uuidv7` and `ulid` keys are sixteen bytes and their digest
    /// covers the first eight, so no digest determines a key.
    fn max_rebuilt_key_len(&self) -> Option<usize> {
        None
    }

    /// Whether [`Self::compare`] is plain byte-lexicographic order.
    ///
    /// Prefix queries are defined in terms of byte prefixes, so they are only
    /// meaningful for encodings where that matches the key order. `i64` keys, for
    /// instance, are little-endian and do not order by their bytes.
    fn is_byte_lexicographic(&self) -> bool {
        false
    }

    /// Checks that `key` is a well-formed key of this encoding.
    fn validate_key(&self, key: &[u8]) -> Result<()> {
        match self.fixed_key_len() {
            Some(len) if key.len() != len => Err(Error::invalid(format!(
                "encoding `{}` needs {len} byte keys, got {}",
                self.id(),
                key.len()
            ))),
            _ => Ok(()),
        }
    }

    /// Human-readable rendering of a key, for diagnostics and the CLI.
    fn format_key(&self, key: &[u8]) -> String {
        key.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Reads back what [`KeyEncoding::format_key`] wrote.
    ///
    /// The two belong together: a key shown to someone is a key they will hand back, and
    /// an encoding that renders keys its own way is the only thing that knows how to
    /// read that rendering. The default pair is lowercase hex.
    fn parse_key(&self, text: &str) -> Result<Vec<u8>> {
        parse_hex_key(text)
    }
}

/// Reads a lowercase or uppercase hex string, which is what the default rendering is.
pub fn parse_hex_key(text: &str) -> Result<Vec<u8>> {
    if text.len() % 2 != 0 {
        return Err(Error::invalid(format!(
            "a hex key needs an even number of digits, got {}",
            text.len()
        )));
    }
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(text.len() / 2);
    for pair in bytes.chunks(2) {
        out.push(hex_digit(pair[0])? << 4 | hex_digit(pair[1])?);
    }
    Ok(out)
}

fn hex_digit(byte: u8) -> Result<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(Error::invalid(format!(
            "`{}` is not a hex digit",
            char::from(byte)
        ))),
    }
}

fn need_len(id: &str, key: &[u8], len: usize) -> Result<()> {
    if key.len() != len {
        return Err(Error::corrupt(format!(
            "encoding `{id}` needs {len} byte keys, got {}",
            key.len()
        )));
    }
    Ok(())
}

/// Little-endian signed 64-bit integer keys (`i64`).
///
/// The digest flips the sign bit, which maps the signed order onto the unsigned digest
/// order, so it is exact and pages built with it store no key bytes.
#[derive(Debug, Default, Clone, Copy)]
pub struct Int64Encoding;

impl Int64Encoding {
    /// Encodes an `i64` key.
    pub fn encode(value: i64) -> [u8; 8] {
        value.to_le_bytes()
    }

    /// Decodes an `i64` key.
    pub fn decode(key: &[u8]) -> Result<i64> {
        need_len("i64", key, 8)?;
        Ok(i64::from_le_bytes(key.try_into().expect("checked above")))
    }
}

impl KeyEncoding for Int64Encoding {
    fn id(&self) -> &str {
        "i64"
    }

    fn compare(&self, a: &[u8], b: &[u8]) -> Result<Ordering> {
        Ok(Int64Encoding::decode(a)?.cmp(&Int64Encoding::decode(b)?))
    }

    fn digest(&self, key: &[u8]) -> Result<u64> {
        Ok((Int64Encoding::decode(key)? as u64) ^ 0x8000_0000_0000_0000)
    }

    fn is_digest_exact(&self) -> bool {
        true
    }

    fn decode_key_from_digest(&self, digest: u64, out: &mut Vec<u8>) -> Result<()> {
        out.clear();
        out.extend_from_slice(&((digest ^ 0x8000_0000_0000_0000) as i64).to_le_bytes());
        Ok(())
    }

    /// The digest is the whole key, so a rebuilt one is always eight bytes.
    fn max_rebuilt_key_len(&self) -> Option<usize> {
        Some(8)
    }

    fn fixed_key_len(&self) -> Option<usize> {
        Some(8)
    }

    fn format_key(&self, key: &[u8]) -> String {
        match Int64Encoding::decode(key) {
            Ok(v) => v.to_string(),
            Err(_) => "<invalid i64 key>".to_string(),
        }
    }

    fn parse_key(&self, text: &str) -> Result<Vec<u8>> {
        let value: i64 = text.trim().parse().map_err(|_| {
            Error::invalid(format!("`{text}` is not a 64-bit signed decimal integer"))
        })?;
        Ok(Int64Encoding::encode(value).to_vec())
    }
}

/// Byte-lexicographic keys (`ascii`).
///
/// The digest is the first eight bytes packed big-endian and zero padded, so keys that
/// share an eight byte prefix collide and fall back to the full comparison.
#[derive(Debug, Default, Clone, Copy)]
pub struct AsciiEncoding;

impl KeyEncoding for AsciiEncoding {
    fn id(&self) -> &str {
        "ascii"
    }

    fn compare(&self, a: &[u8], b: &[u8]) -> Result<Ordering> {
        Ok(a.cmp(b))
    }

    fn digest(&self, key: &[u8]) -> Result<u64> {
        Ok(ascii_digest(key))
    }

    fn is_byte_lexicographic(&self) -> bool {
        true
    }

    fn format_key(&self, key: &[u8]) -> String {
        // Escaped rather than rendered lossily: these keys are byte strings, and
        // `from_utf8_lossy` turns every byte it cannot read into the same character, so
        // two different keys print the same and neither can be handed back.
        let mut out = String::with_capacity(key.len());
        for &byte in key {
            match byte {
                b'\\' => out.push_str("\\\\"),
                0x20..=0x7e => out.push(char::from(byte)),
                _ => out.push_str(&format!("\\x{byte:02x}")),
            }
        }
        out
    }

    fn parse_key(&self, text: &str) -> Result<Vec<u8>> {
        let bytes = text.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] != b'\\' {
                out.push(bytes[i]);
                i += 1;
                continue;
            }
            match bytes.get(i + 1) {
                Some(b'\\') => {
                    out.push(b'\\');
                    i += 2;
                }
                Some(b'x') => {
                    let digits = bytes.get(i + 2..i + 4).ok_or_else(|| {
                        Error::invalid("`\\x` needs two hex digits after it".to_string())
                    })?;
                    out.push(hex_digit(digits[0])? << 4 | hex_digit(digits[1])?);
                    i += 4;
                }
                _ => {
                    return Err(Error::invalid(
                        "a backslash in a key is written `\\\\`, and a byte `\\xNN`".to_string(),
                    ))
                }
            }
        }
        Ok(out)
    }
}

/// The `ascii` digest: first eight bytes, big-endian, zero padded.
pub fn ascii_digest(key: &[u8]) -> u64 {
    let mut digest = 0u64;
    for (i, b) in key.iter().take(8).enumerate() {
        digest |= (*b as u64) << (56 - i * 8);
    }
    digest
}

/// UUIDv7 keys in .NET's `Guid` byte layout (`uuidv7`).
///
/// The 16 key bytes are what `Guid.TryWriteBytes` produces: the first three fields are
/// little-endian (`u32` at 0, `u16` at 4, `u16` at 6) and bytes 8..16 are stored in
/// order. `Guid.CompareTo` compares those three fields as unsigned integers and then
/// the trailing bytes lexicographically; this was verified against .NET 10 rather than
/// inferred (see `docs/compatibility.md`). Use [`Uuidv7Encoding::from_rfc4122`] to
/// convert a canonical big-endian UUID into this layout.
#[derive(Debug, Default, Clone, Copy)]
pub struct Uuidv7Encoding;

impl Uuidv7Encoding {
    /// Converts RFC 4122 (big-endian) bytes into the .NET `Guid` layout used on disk.
    pub fn from_rfc4122(bytes: &[u8; 16]) -> [u8; 16] {
        let mut out = *bytes;
        out[0..4].reverse();
        out[4..6].reverse();
        out[6..8].reverse();
        out
    }

    /// Converts the on-disk .NET `Guid` layout back to RFC 4122 byte order.
    pub fn to_rfc4122(bytes: &[u8; 16]) -> [u8; 16] {
        // The transform is its own inverse.
        Uuidv7Encoding::from_rfc4122(bytes)
    }

    fn fields(key: &[u8]) -> Result<(u32, u16, u16)> {
        need_len("uuidv7", key, 16)?;
        Ok((
            u32::from_le_bytes(key[0..4].try_into().expect("checked")),
            u16::from_le_bytes(key[4..6].try_into().expect("checked")),
            u16::from_le_bytes(key[6..8].try_into().expect("checked")),
        ))
    }
}

impl KeyEncoding for Uuidv7Encoding {
    fn id(&self) -> &str {
        "uuidv7"
    }

    fn compare(&self, a: &[u8], b: &[u8]) -> Result<Ordering> {
        let (aa, ab, ac) = Uuidv7Encoding::fields(a)?;
        let (ba, bb, bc) = Uuidv7Encoding::fields(b)?;
        Ok(aa
            .cmp(&ba)
            .then(ab.cmp(&bb))
            .then(ac.cmp(&bc))
            .then_with(|| a[8..].cmp(&b[8..])))
    }

    fn digest(&self, key: &[u8]) -> Result<u64> {
        let (a, b, c) = Uuidv7Encoding::fields(key)?;
        Ok(((a as u64) << 32) | ((b as u64) << 16) | c as u64)
    }

    fn fixed_key_len(&self) -> Option<usize> {
        Some(16)
    }

    fn format_key(&self, key: &[u8]) -> String {
        if key.len() != 16 {
            return "<invalid uuid key>".to_string();
        }
        let mut b = [0u8; 16];
        b.copy_from_slice(key);
        let r = Uuidv7Encoding::to_rfc4122(&b);
        format!(
            "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{}",
            r[0],
            r[1],
            r[2],
            r[3],
            r[4],
            r[5],
            r[6],
            r[7],
            r[8],
            r[9],
            r[10..]
                .iter()
                .map(|x| format!("{x:02x}"))
                .collect::<String>()
        )
    }

    fn parse_key(&self, text: &str) -> Result<Vec<u8>> {
        // The rendering is the RFC 4122 text form, so that is what reads back. The bare
        // hex of the stored bytes is taken too, because that is what the file holds and
        // what a tool that dumped it would have written.
        let trimmed = text.trim();
        if !trimmed.contains('-') {
            return parse_hex_key(trimmed);
        }
        let digits: String = trimmed.chars().filter(|c| *c != '-').collect();
        if digits.len() != 32 {
            return Err(Error::invalid(format!(
                "`{trimmed}` is not a uuid: 32 hex digits, got {}",
                digits.len()
            )));
        }
        let rfc = parse_hex_key(&digits)?;
        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(&rfc);
        Ok(Uuidv7Encoding::from_rfc4122(&bytes).to_vec())
    }
}

/// ULID keys (`ulid`): 16 bytes compared lexicographically.
///
/// The digest is the first eight bytes read big-endian. Matches
/// `DryDB.UlidKey.UlidKeyEncoding`, whose ordering was verified against
/// `Cysharp/Ulid` 1.3.4.
#[derive(Debug, Default, Clone, Copy)]
pub struct UlidEncoding;

impl KeyEncoding for UlidEncoding {
    fn id(&self) -> &str {
        "ulid"
    }

    /// Byte order, for any length.
    ///
    /// Keys are sixteen bytes, which [`KeyEncoding::validate_key`] enforces. Comparison
    /// and digest accept shorter inputs so that a byte prefix, which is not a key, can
    /// still be placed in the same order as the keys it selects.
    fn compare(&self, a: &[u8], b: &[u8]) -> Result<Ordering> {
        Ok(a.cmp(b))
    }

    fn digest(&self, key: &[u8]) -> Result<u64> {
        Ok(ascii_digest(key))
    }

    fn fixed_key_len(&self) -> Option<usize> {
        Some(16)
    }

    fn is_byte_lexicographic(&self) -> bool {
        true
    }
}

/// Composite `(source key, rid)` encoding used by non-unique secondary index trees.
///
/// The tree itself only stores unique keys, so a duplicate secondary key is made unique
/// by appending a 4-byte little-endian record id. The source key dominates the order,
/// so the source digest stays order preserving for the composite key.
pub struct DuplicateKeyEncoding {
    source: Arc<dyn KeyEncoding>,
}

impl std::fmt::Debug for DuplicateKeyEncoding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DuplicateKeyEncoding")
            .field("source", &self.source.id())
            .finish()
    }
}

/// Width of the record id appended to a non-unique secondary key.
pub const RID_LEN: usize = 4;

impl DuplicateKeyEncoding {
    /// Wraps a source encoding.
    pub fn new(source: Arc<dyn KeyEncoding>) -> Self {
        DuplicateKeyEncoding { source }
    }

    /// The wrapped encoding.
    pub fn source(&self) -> &Arc<dyn KeyEncoding> {
        &self.source
    }

    /// Builds `source_key || rid`.
    pub fn encode(source_key: &[u8], rid: i32) -> Vec<u8> {
        let mut out = Vec::with_capacity(source_key.len() + RID_LEN);
        out.extend_from_slice(source_key);
        out.extend_from_slice(&rid.to_le_bytes());
        out
    }

    /// Splits a composite key into `(source_key, rid)`.
    pub fn split(key: &[u8]) -> Result<(&[u8], i32)> {
        if key.len() < RID_LEN {
            return Err(Error::corrupt(format!(
                "duplicate key is {} bytes, shorter than the {RID_LEN} byte record id",
                key.len()
            )));
        }
        let split = key.len() - RID_LEN;
        let rid = i32::from_le_bytes(key[split..].try_into().expect("checked above"));
        if rid < 0 {
            // Record ids count up from zero within an index key, and a lookup is the
            // composite range `(k, 0)..=(k, i32::MAX)`. A negative one sorts below that
            // whole range, so the row it belongs to is on the page and in the count but
            // outside every lookup.
            return Err(Error::corrupt(format!(
                "duplicate key has record id {rid}, which is negative"
            )));
        }
        Ok((&key[..split], rid))
    }
}

impl KeyEncoding for DuplicateKeyEncoding {
    fn id(&self) -> &str {
        self.source.id()
    }

    fn compare(&self, a: &[u8], b: &[u8]) -> Result<Ordering> {
        let (a_key, a_rid) = DuplicateKeyEncoding::split(a)?;
        let (b_key, b_rid) = DuplicateKeyEncoding::split(b)?;
        Ok(self.source.compare(a_key, b_key)?.then(a_rid.cmp(&b_rid)))
    }

    fn digest(&self, key: &[u8]) -> Result<u64> {
        let (source_key, _) = DuplicateKeyEncoding::split(key)?;
        self.source.digest(source_key)
    }

    /// Accepts either convention for a non-unique index page.
    ///
    /// The upstream builder digests the whole composite key while its reader digests
    /// only the source part, and the two agree only when the source key already fills
    /// the eight digest bytes. Both are order preserving, so a page written either way
    /// is read here; `docs/compatibility.md` D1 records what that difference costs on
    /// upstream's own reader.
    fn accepts_digest(&self, key: &[u8], stored: u64) -> Result<bool> {
        if self.digest(key)? == stored {
            return Ok(true);
        }
        Ok(self.source.digest(key)? == stored)
    }

    /// Never exact: two rows with the same index key share a digest and differ only in
    /// their record id.
    fn is_digest_exact(&self) -> bool {
        false
    }

    /// Rebuilds `source_key || 0`.
    ///
    /// The upstream builder writes non-unique index pages with the keys omitted
    /// whenever the *source* encoding has an exact digest, even though the record id it
    /// appends is not part of that digest. On such a page the record id genuinely is not
    /// stored, so the best that can be recovered is the source key with a zero record
    /// id. Nothing reachable from the public API depends on the difference: an index
    /// cursor reports the source key and drops the record id.
    fn decode_key_from_digest(&self, digest: u64, out: &mut Vec<u8>) -> Result<()> {
        self.source.decode_key_from_digest(digest, out)?;
        out.extend_from_slice(&0i32.to_le_bytes());
        Ok(())
    }

    fn fixed_key_len(&self) -> Option<usize> {
        self.source.fixed_key_len().map(|n| n + RID_LEN)
    }

    /// The source's bound plus the record id this encoding appends.
    ///
    /// Separate from [`KeyEncoding::fixed_key_len`], which the default would use: a
    /// source encoding whose keys vary in length can still state a bound, and a
    /// non-unique index over it has to inherit that rather than fall back to "unknown".
    fn max_rebuilt_key_len(&self) -> Option<usize> {
        self.source.max_rebuilt_key_len().map(|n| n + RID_LEN)
    }

    fn format_key(&self, key: &[u8]) -> String {
        match DuplicateKeyEncoding::split(key) {
            Ok((source_key, rid)) => format!("{}-{rid}", self.source.format_key(source_key)),
            Err(_) => "<invalid duplicate key>".to_string(),
        }
    }
}

/// Key bytes an error message may describe.
///
/// A key is as long as the file says it is, and a message that renders two of them in
/// full is memory spent outside the budget before anything can cut it back down. Enough
/// to tell two keys apart in a diagnostic is enough.
const DESCRIBE_KEY_BYTES: usize = 48;

/// Renders a key for an error message, bounded.
pub(crate) fn describe_key(encoding: &dyn KeyEncoding, key: &[u8]) -> String {
    if key.len() <= DESCRIBE_KEY_BYTES {
        return encoding.format_key(key);
    }
    let mut short = encoding.format_key(&key[..DESCRIBE_KEY_BYTES]);
    short.push_str(&format!("... ({} bytes)", key.len()));
    short
}

/// Resolves key encoding ids to implementations.
///
/// Built-in ids resolve without registration. Custom encodings are registered per
/// database through [`OpenOptions`](crate::OpenOptions), so no process-wide mutable
/// state decides how a file is read.
#[derive(Clone, Default)]
pub struct EncodingRegistry {
    custom: HashMap<String, Arc<dyn KeyEncoding>>,
}

impl std::fmt::Debug for EncodingRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncodingRegistry")
            .field("custom", &self.custom.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl EncodingRegistry {
    /// An empty registry; built-ins still resolve.
    pub fn new() -> Self {
        EncodingRegistry::default()
    }

    /// Registers a custom encoding, replacing any previous one with the same id.
    ///
    /// Built-in ids cannot be shadowed: a file that says `i64` always means the
    /// built-in `i64`.
    pub fn register(&mut self, encoding: Arc<dyn KeyEncoding>) -> Result<()> {
        let id = encoding.id().to_string();
        if builtin(&id).is_some() {
            return Err(Error::invalid(format!(
                "`{id}` is a built-in key encoding and cannot be replaced"
            )));
        }
        self.custom.insert(id, encoding);
        Ok(())
    }

    /// Resolves an id, or reports [`ErrorKind::UnknownEncoding`].
    pub fn resolve(&self, id: &str) -> Result<Arc<dyn KeyEncoding>> {
        if let Some(e) = builtin(id) {
            return Ok(e);
        }
        self.custom.get(id).cloned().ok_or_else(|| {
            Error::new(
                ErrorKind::UnknownEncoding,
                format!("key encoding `{id}` is not registered"),
            )
        })
    }
}

fn builtin(id: &str) -> Option<Arc<dyn KeyEncoding>> {
    match id {
        "i64" => Some(Arc::new(Int64Encoding)),
        "ascii" => Some(Arc::new(AsciiEncoding)),
        "uuidv7" => Some(Arc::new(Uuidv7Encoding)),
        "ulid" => Some(Arc::new(UlidEncoding)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn i64_digest_preserves_order() {
        let e = Int64Encoding;
        let values = [i64::MIN, -2, -1, 0, 1, 2, i64::MAX];
        for w in values.windows(2) {
            let a = Int64Encoding::encode(w[0]);
            let b = Int64Encoding::encode(w[1]);
            assert_eq!(e.compare(&a, &b).unwrap(), Ordering::Less);
            assert!(e.digest(&a).unwrap() < e.digest(&b).unwrap(), "{:?}", w);
        }
    }

    #[test]
    fn i64_digest_round_trips() {
        let e = Int64Encoding;
        let mut out = Vec::new();
        for v in [i64::MIN, -1, 0, 1, i64::MAX, 1234567] {
            let key = Int64Encoding::encode(v);
            e.decode_key_from_digest(e.digest(&key).unwrap(), &mut out)
                .unwrap();
            assert_eq!(out, key);
        }
    }

    #[test]
    fn i64_rejects_wrong_width() {
        let e = Int64Encoding;
        assert!(e.digest(&[0u8; 7]).is_err());
        assert!(e.validate_key(&[0u8; 9]).is_err());
    }

    #[test]
    fn ascii_digest_preserves_order() {
        let e = AsciiEncoding;
        let keys: Vec<&[u8]> = vec![b"", b"a", b"aa", b"ab", b"abcdefgh", b"abcdefghi", b"b"];
        for w in keys.windows(2) {
            assert_eq!(e.compare(w[0], w[1]).unwrap(), Ordering::Less);
            assert!(e.digest(w[0]).unwrap() <= e.digest(w[1]).unwrap());
        }
        // Keys sharing an eight byte prefix collide.
        assert_eq!(
            e.digest(b"abcdefgh").unwrap(),
            e.digest(b"abcdefghi").unwrap()
        );
    }

    #[test]
    fn uuid_fields_compare_unsigned() {
        // Verified against .NET 10: Guid.CompareTo treats the leading fields as
        // unsigned, so 0x7f000000 sorts below 0x80000000.
        let e = Uuidv7Encoding;
        let mut lo = [0u8; 16];
        lo[3] = 0x7f;
        let mut hi = [0u8; 16];
        hi[3] = 0x80;
        assert_eq!(e.compare(&lo, &hi).unwrap(), Ordering::Less);
        assert!(e.digest(&lo).unwrap() < e.digest(&hi).unwrap());
    }

    #[test]
    fn uuid_rfc4122_conversion_round_trips() {
        let rfc = [
            0x01, 0x89, 0x0a, 0x5d, 0xac, 0x96, 0x77, 0x4b, 0xbc, 0xce, 0xb3, 0x02, 0x09, 0x9a,
            0x80, 0x57,
        ];
        let dotnet = Uuidv7Encoding::from_rfc4122(&rfc);
        // Matches Guid.TryWriteBytes of 01890a5d-ac96-774b-bcce-b302099a8057 as
        // observed on .NET 10.
        assert_eq!(
            dotnet,
            [
                0x5d, 0x0a, 0x89, 0x01, 0x96, 0xac, 0x4b, 0x77, 0xbc, 0xce, 0xb3, 0x02, 0x09, 0x9a,
                0x80, 0x57
            ]
        );
        assert_eq!(Uuidv7Encoding::to_rfc4122(&dotnet), rfc);
    }

    #[test]
    fn duplicate_key_orders_by_source_then_rid() {
        let e = DuplicateKeyEncoding::new(Arc::new(AsciiEncoding));
        let a = DuplicateKeyEncoding::encode(b"k", 0);
        let b = DuplicateKeyEncoding::encode(b"k", 1);
        let c = DuplicateKeyEncoding::encode(b"l", 0);
        assert_eq!(e.compare(&a, &b).unwrap(), Ordering::Less);
        assert_eq!(e.compare(&b, &c).unwrap(), Ordering::Less);
        assert_eq!(e.digest(&a).unwrap(), e.digest(&b).unwrap());
    }

    #[test]
    fn registry_rejects_unknown_and_shadowing() {
        let mut r = EncodingRegistry::new();
        assert_eq!(
            r.resolve("nope").unwrap_err().kind(),
            ErrorKind::UnknownEncoding
        );
        assert!(r.resolve("i64").is_ok());

        #[derive(Debug)]
        struct Shadow;
        impl KeyEncoding for Shadow {
            fn id(&self) -> &str {
                "i64"
            }
            fn compare(&self, _: &[u8], _: &[u8]) -> Result<Ordering> {
                Ok(Ordering::Equal)
            }
            fn digest(&self, _: &[u8]) -> Result<u64> {
                Ok(0)
            }
        }
        assert!(r.register(Arc::new(Shadow)).is_err());
    }
}
