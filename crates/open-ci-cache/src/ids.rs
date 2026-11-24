//! Validated identifiers and byte ranges shared by the HTTP layer, index, and blob stores.

use crate::error::CacheError;
use sha2::{Digest, Sha256};
use std::fmt;
use uuid::Uuid;

const MAX_BLOB_ID_LEN: usize = 128;
const MAX_BLOCK_ID_LEN: usize = 256;

/// Opaque server-minted blob identifier, safe to embed in file paths and object keys.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BlobId(String);

impl BlobId {
    pub fn generate() -> Self {
        Self(Uuid::new_v4().to_string())
    }

    pub fn parse(raw: &str) -> Result<Self, CacheError> {
        let valid = !raw.is_empty()
            && raw.len() <= MAX_BLOB_ID_LEN
            && raw
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'));
        if valid {
            Ok(Self(raw.to_owned()))
        } else {
            Err(CacheError::BadRequest("invalid blob id".to_owned()))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BlobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Client-chosen Azure block identifier from `?comp=block&blockid=...`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BlockId(String);

impl BlockId {
    pub fn parse(raw: &str) -> Result<Self, CacheError> {
        let valid =
            !raw.is_empty() && raw.len() <= MAX_BLOCK_ID_LEN && !raw.chars().any(char::is_control);
        if valid {
            Ok(Self(raw.to_owned()))
        } else {
            Err(CacheError::BadRequest("invalid block id".to_owned()))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Fixed-length hex digest, safe as a file name or object key segment for any client input.
    pub fn storage_name(&self) -> String {
        hex::encode(Sha256::digest(self.0.as_bytes()))
    }
}

/// Inclusive byte range already resolved against a known, non-empty blob size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ByteRange {
    start: u64,
    end_inclusive: u64,
}

impl ByteRange {
    /// The whole blob, or `None` for an empty blob which has no addressable bytes.
    pub fn full(size: u64) -> Option<Self> {
        size.checked_sub(1).map(|last| Self {
            start: 0,
            end_inclusive: last,
        })
    }

    /// Resolves a single-range HTTP `Range` header (`bytes=a-b`, `bytes=a-`, `bytes=-n`).
    pub fn parse_header(header: &str, size: u64) -> Result<Self, CacheError> {
        let unsatisfiable = |msg: &str| CacheError::RangeNotSatisfiable(msg.to_owned());
        let last = size
            .checked_sub(1)
            .ok_or_else(|| unsatisfiable("empty blob has no byte ranges"))?;
        let spec = header
            .trim()
            .strip_prefix("bytes=")
            .ok_or_else(|| unsatisfiable("unsupported range unit"))?;
        if spec.contains(',') {
            return Err(unsatisfiable("multipart byte ranges are not supported"));
        }
        let (start_raw, end_raw) = spec
            .split_once('-')
            .ok_or_else(|| unsatisfiable("malformed byte range"))?;
        let parse = |raw: &str| {
            raw.parse::<u64>()
                .map_err(|_| unsatisfiable("malformed byte range"))
        };
        let (start, end_inclusive) = match (start_raw.is_empty(), end_raw.is_empty()) {
            (true, false) => {
                let suffix = parse(end_raw)?;
                if suffix == 0 {
                    return Err(unsatisfiable("suffix length must be positive"));
                }
                (size.saturating_sub(suffix), last)
            }
            (false, true) => (parse(start_raw)?, last),
            (false, false) => (parse(start_raw)?, parse(end_raw)?.min(last)),
            (true, true) => return Err(unsatisfiable("malformed byte range")),
        };
        if start > end_inclusive {
            return Err(unsatisfiable("requested byte range out of bounds"));
        }
        Ok(Self {
            start,
            end_inclusive,
        })
    }

    pub fn start(self) -> u64 {
        self.start
    }

    pub fn end_inclusive(self) -> u64 {
        self.end_inclusive
    }

    pub fn byte_count(self) -> u64 {
        self.end_inclusive - self.start + 1
    }

    /// `Range` request header value, e.g. `bytes=0-99`.
    pub fn to_header_value(self) -> String {
        format!("bytes={}-{}", self.start, self.end_inclusive)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_id_rejects_path_traversal_and_separators() {
        for bad in ["", "../x", "a/b", "a.b", "a b", &"x".repeat(129)] {
            assert!(BlobId::parse(bad).is_err(), "{bad:?} must be rejected");
        }
        assert!(BlobId::parse(BlobId::generate().as_str()).is_ok());
    }

    #[test]
    fn block_storage_name_is_fixed_length_hex_for_any_input() {
        let name = BlockId::parse("../../etc/passwd").map(|b| b.storage_name());
        assert!(matches!(name, Ok(n) if n.len() == 64 && n.bytes().all(|b| b.is_ascii_hexdigit())));
    }

    #[test]
    fn byte_range_resolves_every_single_range_form() {
        let r = |h: &str| ByteRange::parse_header(h, 100).map(|r| (r.start(), r.end_inclusive()));
        assert!(matches!(r("bytes=0-9"), Ok((0, 9))));
        assert!(matches!(r("bytes=90-"), Ok((90, 99))));
        assert!(matches!(r("bytes=-10"), Ok((90, 99))));
        assert!(matches!(r("bytes=50-500"), Ok((50, 99))));
        assert!(matches!(r("bytes=-500"), Ok((0, 99))));
        for bad in [
            "bytes=100-",
            "bytes=9-1",
            "bytes=-0",
            "items=0-1",
            "bytes=0-1,3-4",
            "bytes=-",
        ] {
            assert!(r(bad).is_err(), "{bad} must be unsatisfiable");
        }
        assert!(ByteRange::parse_header("bytes=0-0", 0).is_err());
        assert_eq!(ByteRange::full(0), None);
        assert_eq!(ByteRange::full(5).map(ByteRange::byte_count), Some(5));
    }
}
