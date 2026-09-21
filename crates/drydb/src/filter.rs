//! Page filters.
//!
//! A filter transforms a page's bytes after the 28-byte prefix. The prefix (page
//! length plus node header) is always stored raw, which is what lets the builder
//! back-patch sibling pointers into a page it has already compressed, and lets the
//! reader learn a page's stored length with one 4-byte read.
//!
//! # Only one filter per file
//!
//! The header can name several filters, but upstream 1.4 cannot produce or consume
//! such a file: its encoder writes the output of the *first* filter and discards the
//! rest, and its decoder calls `Decode` on the first filter and `Encode` on the others.
//! Rather than invent a chaining order no C# file uses, this crate rejects files that
//! name more than one filter. `docs/compatibility.md` records the defect.

use std::collections::HashMap;
use std::sync::Arc;

use crate::error::{Error, ErrorKind, Result};

/// A reversible transform applied to page payloads.
pub trait PageFilter: std::fmt::Debug + Send + Sync + 'static {
    /// Registry id, stored in the file header.
    fn id(&self) -> &str;

    /// Transforms a page payload on the way out.
    ///
    /// `input` is the page without its 28-byte prefix, which is always stored raw.
    /// `out` is empty; write the encoded payload into it.
    fn encode(&self, input: &[u8], out: &mut Vec<u8>) -> Result<()>;

    /// Memory this filter needs of its own while decoding one page.
    ///
    /// Reserved around a decode, on top of the decoded page itself. A filter that calls
    /// into a library allocating through something other than Rust's allocator is memory
    /// this crate cannot see but can still account for, provided the filter says how
    /// much: zstd's decompression context is such a case.
    fn working_set_bytes(&self) -> u64 {
        0
    }

    /// What asking [`PageFilter::working_set_bytes`] costs while it answers.
    ///
    /// Zero for a filter that knows its own answer. A filter that has to measure
    /// something allocates to do it, and a reader that reserved only the answer would
    /// have gone over the budget asking the question. The reader reserves this first and
    /// refuses the file if it does not fit, so a budget too small for the filter never
    /// allocates anything for it.
    fn probe_bytes(&self) -> u64 {
        0
    }

    /// How large the decoded payload will be, when the encoded form says so.
    ///
    /// Returning `Some` lets the reader reserve exactly that much instead of the
    /// configured decoded-page limit, which keeps a compressed database readable inside
    /// a modest memory budget. Returning `None` is always allowed; the reader then
    /// reserves the limit.
    fn decoded_size_hint(&self, input: &[u8]) -> Option<usize> {
        let _ = input;
        None
    }

    /// Reverses [`Self::encode`].
    ///
    /// `out` is empty and already has the configured decoded-page limit reserved.
    /// Write the decoded payload into it **without growing it**: a filter that would
    /// need more room must return an error instead, so that a crafted page cannot turn
    /// into an unbounded allocation. The caller checks the capacity afterwards and
    /// rejects the page if it grew.
    fn decode(&self, input: &[u8], out: &mut Vec<u8>) -> Result<()>;
}

/// Resolves filter ids to implementations.
///
/// The built-in `DryDB.ZstdCompression` filter is available when the `zstd` feature is
/// on. Custom filters are registered per database on
/// [`OpenOptions`](crate::OpenOptions).
#[derive(Clone, Default)]
pub struct FilterRegistry {
    custom: HashMap<String, Arc<dyn PageFilter>>,
}

impl std::fmt::Debug for FilterRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FilterRegistry")
            .field("custom", &self.custom.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl FilterRegistry {
    /// An empty registry; built-ins still resolve.
    pub fn new() -> FilterRegistry {
        FilterRegistry::default()
    }

    /// Registers a filter, replacing any previous one with the same id.
    pub fn register(&mut self, filter: Arc<dyn PageFilter>) {
        self.custom.insert(filter.id().to_string(), filter);
    }

    /// Resolves an id, or reports [`ErrorKind::UnknownFilter`].
    pub fn resolve(&self, id: &str) -> Result<Arc<dyn PageFilter>> {
        if let Some(f) = self.custom.get(id) {
            return Ok(Arc::clone(f));
        }
        #[cfg(feature = "zstd")]
        if id == ZSTD_FILTER_ID {
            return Ok(Arc::new(ZstdFilter::default()));
        }
        Err(Error::new(
            ErrorKind::UnknownFilter,
            format!(
                "page filter `{id}` is not registered{}",
                if cfg!(feature = "zstd") {
                    ""
                } else {
                    " (build with the `zstd` feature for DryDB.ZstdCompression)"
                }
            ),
        ))
    }
}

/// Id of the upstream zstd page filter.
pub const ZSTD_FILTER_ID: &str = "DryDB.ZstdCompression";

#[cfg(feature = "zstd")]
mod zstd_impl {
    use super::*;
    use std::sync::OnceLock;

    /// The upstream `DryDB.ZstdCompression` filter: one raw zstd frame per page
    /// payload, no extra framing.
    #[derive(Debug, Clone)]
    pub struct ZstdFilter {
        level: i32,
    }

    impl Default for ZstdFilter {
        fn default() -> Self {
            ZstdFilter::with_level(zstd_safe::CLEVEL_DEFAULT)
        }
    }

    impl ZstdFilter {
        /// A filter compressing at `level`. The level only affects writing.
        pub fn with_level(level: i32) -> ZstdFilter {
            ZstdFilter { level }
        }
    }

    /// The smallest figure this filter will claim, and what the measurement is allowed
    /// to cost.
    ///
    /// A floor, so a measurement that comes back implausibly small still reserves
    /// something of the right order, and an upper bound on the context the measurement
    /// itself makes.
    const WORKING_SET_FLOOR: u64 = 128 * 1024;

    /// What one decompression context costs, asked of zstd itself, once per process.
    ///
    /// zstd allocates its context through C's allocator, so nothing on the Rust side
    /// sees it. The figure is fixed for a given build, so it is measured once and kept:
    /// a database that is opened and closed repeatedly would otherwise pay for the
    /// measurement every time. Reading it from zstd rather than writing a number down
    /// keeps it right across versions and platforms. Measured at 95,984 bytes on
    /// aarch64 macOS with zstd-safe 7.3, which is under the floor below, so the floor is
    /// what this build declares.
    static WORKING_SET: OnceLock<u64> = OnceLock::new();

    /// A zstd frame holding the five bytes `drydb`, written at the default level.
    ///
    /// A constant rather than something compressed on the spot, so that measuring the
    /// decompression context does not build a compression context beside it. What the
    /// measurement costs is reserved before it runs, and the reservation has to bound
    /// it.
    const PROBE_FRAME: [u8; 18] = [
        0x28, 0xb5, 0x2f, 0xfd, 0x24, 0x05, 0x29, 0x00, 0x00, 0x64, 0x72, 0x79, 0x64, 0x62, 0xb2,
        0xf5, 0x52, 0xf5,
    ];

    fn working_set() -> u64 {
        *WORKING_SET.get_or_init(|| {
            let mut context = zstd_safe::DCtx::create();
            // Decoded once first, so the figure covers a context that has done the work
            // rather than one that might still allocate on its first frame. On this
            // build it reports 95,984 bytes either way.
            let mut out = Vec::with_capacity(8);
            let _ = context.decompress(&mut out, &PROBE_FRAME);
            (context.sizeof() as u64).max(WORKING_SET_FLOOR)
        })
    }

    impl PageFilter for ZstdFilter {
        fn id(&self) -> &str {
            ZSTD_FILTER_ID
        }

        fn working_set_bytes(&self) -> u64 {
            working_set()
        }

        fn probe_bytes(&self) -> u64 {
            // Nothing to measure once the figure is known.
            if WORKING_SET.get().is_some() {
                0
            } else {
                WORKING_SET_FLOOR
            }
        }

        fn encode(&self, input: &[u8], out: &mut Vec<u8>) -> Result<()> {
            // `zstd_safe` writes from the start of the vector's allocation and then
            // sets its length, so the destination has to be empty.
            out.clear();
            out.reserve(zstd_safe::compress_bound(input.len()));
            zstd_safe::compress(out, input, self.level).map_err(|code| {
                Error::new(
                    ErrorKind::Io,
                    format!(
                        "zstd compression failed: {}",
                        zstd_safe::get_error_name(code)
                    ),
                )
            })?;
            Ok(())
        }

        fn decoded_size_hint(&self, input: &[u8]) -> Option<usize> {
            // A frame written by `ZSTD_compress` records its content size. One written
            // by a streaming encoder without a pledged size does not, and this returns
            // `None` for it.
            match zstd_safe::get_frame_content_size(input) {
                Ok(Some(size)) => usize::try_from(size).ok(),
                _ => None,
            }
        }

        fn decode(&self, input: &[u8], out: &mut Vec<u8>) -> Result<()> {
            out.clear();
            let limit = out.capacity();
            // Decompresses straight into the caller's reservation. A frame that needs
            // more than that fails with `dstSize_tooSmall` rather than growing.
            zstd_safe::decompress(out, input).map_err(|code| {
                Error::new(
                    ErrorKind::CorruptData,
                    format!(
                        "zstd decompression failed: {} (decoded page limit is {limit} bytes)",
                        zstd_safe::get_error_name(code)
                    ),
                )
            })?;
            Ok(())
        }
    }
}

#[cfg(feature = "zstd")]
pub use zstd_impl::ZstdFilter;

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Xor;

    impl PageFilter for Xor {
        fn id(&self) -> &str {
            "test.xor"
        }
        fn encode(&self, input: &[u8], out: &mut Vec<u8>) -> Result<()> {
            out.extend(input.iter().map(|b| b ^ 0x5a));
            Ok(())
        }
        fn decode(&self, input: &[u8], out: &mut Vec<u8>) -> Result<()> {
            self.encode(input, out)
        }
    }

    #[test]
    fn unknown_filter_is_rejected() {
        let registry = FilterRegistry::new();
        assert_eq!(
            registry.resolve("nope").unwrap_err().kind(),
            ErrorKind::UnknownFilter
        );
    }

    #[test]
    fn custom_filters_resolve() {
        let mut registry = FilterRegistry::new();
        registry.register(Arc::new(Xor));
        let f = registry.resolve("test.xor").unwrap();
        let mut encoded = Vec::new();
        f.encode(b"hello", &mut encoded).unwrap();
        let mut decoded = Vec::with_capacity(64);
        f.decode(&encoded, &mut decoded).unwrap();
        assert_eq!(decoded, b"hello");
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn zstd_round_trips() {
        let f = ZstdFilter::default();
        let payload = b"the same sentence over and over. ".repeat(64);
        let mut encoded = Vec::new();
        f.encode(&payload, &mut encoded).unwrap();
        assert!(encoded.len() < payload.len());
        let mut decoded = Vec::with_capacity(payload.len() * 2);
        f.decode(&encoded, &mut decoded).unwrap();
        assert_eq!(decoded, payload);
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn zstd_refuses_to_exceed_the_decoded_limit() {
        let f = ZstdFilter::default();
        let payload = vec![7u8; 200_000];
        let mut encoded = Vec::new();
        f.encode(&payload, &mut encoded).unwrap();
        let mut decoded = Vec::with_capacity(1024);
        let err = f.decode(&encoded, &mut decoded).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::CorruptData);
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn zstd_resolves_from_the_registry() {
        let registry = FilterRegistry::new();
        assert_eq!(
            registry.resolve(ZSTD_FILTER_ID).unwrap().id(),
            ZSTD_FILTER_ID
        );
    }
}
