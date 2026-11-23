// SPDX-License-Identifier: MPL-2.0
//! On-disk cooked asset wrapper (`docs/specs/12-assets.md` § 4).

use regia_core::id::TypePathHash;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::id::AssetId;

/// Magic bytes at the start of every cooked asset file.
pub const COOKED_MAGIC: [u8; 4] = *b"FCKD";

/// Current cooked wrapper format version.
pub const COOKED_FORMAT_VERSION: u32 = 1;

/// Header stored in cooked files (payload follows immediately after the encoded header block).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CookedHeader {
    /// Must be [`COOKED_FORMAT_VERSION`].
    pub format_version: u32,
    /// Reflect path hash of the payload type (`regia_asset::CpuMesh`, etc.).
    pub type_tag: TypePathHash,
    /// Asset this blob belongs to.
    pub asset_id: AssetId,
    /// Importer semver packed as `major * 1_000_000 + minor * 1_000 + patch`.
    pub importer_version: u32,
    /// Other assets embedded or referenced by this cook.
    pub dependencies: Vec<AssetId>,
    /// Byte length of the payload section.
    pub payload_len: u64,
}

/// Errors reading or writing cooked files.
#[derive(Debug, Error)]
pub enum CookedIoError {
    /// File too small or magic mismatch.
    #[error("not a Regia Engine cooked file (bad magic or truncated header)")]
    BadMagic,

    /// Unsupported [`CookedHeader::format_version`].
    #[error("unsupported cooked format version {found} (supported: {supported})")]
    UnsupportedVersion {
        /// Version in the file.
        found: u32,
        /// Highest version this runtime accepts.
        supported: u32,
    },

    /// Postcard decode failed.
    #[error("cooked header decode error: {0}")]
    Postcard(#[from] postcard::Error),

    /// Payload length does not match file size.
    #[error("cooked payload length {expected} does not match file ({actual} bytes after header)")]
    PayloadLength {
        /// Declared payload length.
        expected: u64,
        /// Bytes available after header.
        actual: u64,
    },

    /// std::io error.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Encode `header` + `payload` into a single byte vector.
pub fn encode_cooked_file(header: &CookedHeader, payload: &[u8]) -> Result<Vec<u8>, CookedIoError> {
    if header.payload_len != payload.len() as u64 {
        return Err(CookedIoError::PayloadLength {
            expected: header.payload_len,
            actual: payload.len() as u64,
        });
    }
    let header_bytes = postcard::to_allocvec(header)?;
    let mut out = Vec::with_capacity(4 + header_bytes.len() + payload.len());
    out.extend_from_slice(&COOKED_MAGIC);
    out.extend_from_slice(&header_bytes);
    out.extend_from_slice(payload);
    Ok(out)
}

/// Split a cooked file into header and payload slices.
pub fn decode_cooked_file(bytes: &[u8]) -> Result<(CookedHeader, &[u8]), CookedIoError> {
    if bytes.len() < 4 || bytes[..4] != COOKED_MAGIC {
        return Err(CookedIoError::BadMagic);
    }
    let (header, rest) = postcard::take_from_bytes::<CookedHeader>(&bytes[4..])?;
    // The format starts at 1: a zero version word is damage, not a current file.
    if header.format_version == 0 || header.format_version > COOKED_FORMAT_VERSION {
        return Err(CookedIoError::UnsupportedVersion {
            found: header.format_version,
            supported: COOKED_FORMAT_VERSION,
        });
    }
    // `try_from`: on a 32-bit target `as usize` would wrap a huge declared length to a small one.
    // Every writer ([`encode_cooked_file`]) stores exactly `payload_len` bytes, so a file with
    // more bytes after the payload is damaged or appended to and is refused like a short one
    // (audit wave 10c, stream L1).
    if usize::try_from(header.payload_len).ok() != Some(rest.len()) {
        return Err(CookedIoError::PayloadLength {
            expected: header.payload_len,
            actual: rest.len() as u64,
        });
    }
    Ok((header, rest))
}

/// Pack semver components into the header field.
///
/// Saturates at `u32::MAX` instead of overflowing (a panic in debug builds, a wrapped, smaller
/// version in release builds) for a `major` of 4295 or more.
pub fn pack_importer_version(major: u32, minor: u32, patch: u32) -> u32 {
    major
        .saturating_mul(1_000_000)
        .saturating_add(minor.saturating_mul(1_000))
        .saturating_add(patch)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_cooked_file() {
        let header = CookedHeader {
            format_version: COOKED_FORMAT_VERSION,
            type_tag: TypePathHash::of("regia_asset::CpuMesh"),
            asset_id: AssetId::new_random(),
            importer_version: pack_importer_version(1, 0, 0),
            dependencies: vec![AssetId::new_random()],
            payload_len: 3,
        };
        let payload = b"abc";
        let file = encode_cooked_file(&header, payload).expect("encode");
        let (h2, p2) = decode_cooked_file(&file).expect("decode");
        assert_eq!(h2, header);
        assert_eq!(p2, payload);
    }
    fn header(version: u32, payload_len: u64) -> CookedHeader {
        CookedHeader {
            format_version: version,
            type_tag: TypePathHash::of("regia_asset::CpuMesh"),
            asset_id: AssetId::new_random(),
            importer_version: pack_importer_version(1, 2, 3),
            dependencies: Vec::new(),
            payload_len,
        }
    }

    /// Bytes of `header` followed by `payload`, built without the length check of
    /// [`encode_cooked_file`] so tests can write inconsistent files.
    fn raw_file(header: &CookedHeader, payload: &[u8]) -> Vec<u8> {
        let mut out = COOKED_MAGIC.to_vec();
        out.extend_from_slice(&postcard::to_allocvec(header).expect("header"));
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn empty_short_and_wrong_magic_are_bad_magic() {
        assert!(matches!(decode_cooked_file(&[]), Err(CookedIoError::BadMagic)));
        assert!(matches!(decode_cooked_file(b"FCK"), Err(CookedIoError::BadMagic)));
        let mut file = raw_file(&header(COOKED_FORMAT_VERSION, 0), &[]);
        file[0] = b'X';
        assert!(matches!(decode_cooked_file(&file), Err(CookedIoError::BadMagic)));
    }

    #[test]
    fn a_newer_wrapper_version_is_rejected() {
        let file = raw_file(&header(99, 0), &[]);
        match decode_cooked_file(&file) {
            Err(CookedIoError::UnsupportedVersion { found, supported }) => {
                assert_eq!(found, 99);
                assert_eq!(supported, COOKED_FORMAT_VERSION);
            }
            other => panic!("expected UnsupportedVersion, got {other:?}"),
        }
    }

    #[test]
    fn a_payload_length_past_the_end_of_the_file_is_rejected() {
        let file = raw_file(&header(COOKED_FORMAT_VERSION, 10), b"abc");
        match decode_cooked_file(&file) {
            Err(CookedIoError::PayloadLength { expected, actual }) => {
                assert_eq!(expected, 10);
                assert_eq!(actual, 3);
            }
            other => panic!("expected PayloadLength, got {other:?}"),
        }
    }

    /// Audit wave 10c (stream L1): every writer stores exactly `payload_len` bytes after the
    /// header, so bytes past the declared payload are damage or an append, not data to ignore.
    #[test]
    fn a_file_with_bytes_after_its_declared_payload_is_refused() {
        let file = raw_file(&header(COOKED_FORMAT_VERSION, 2), b"abcdef");
        match decode_cooked_file(&file) {
            Err(CookedIoError::PayloadLength { expected, actual }) => {
                assert_eq!(expected, 2);
                assert_eq!(actual, 6);
            }
            other => panic!("expected PayloadLength, got {other:?}"),
        }
        let exact = raw_file(&header(COOKED_FORMAT_VERSION, 6), b"abcdef");
        let (_, payload) = decode_cooked_file(&exact).expect("an exact file decodes");
        assert_eq!(payload, b"abcdef");
    }

    #[test]
    fn encode_refuses_a_header_whose_length_disagrees_with_the_payload() {
        assert!(matches!(
            encode_cooked_file(&header(COOKED_FORMAT_VERSION, 4), b"abc"),
            Err(CookedIoError::PayloadLength {
                expected: 4,
                actual: 3
            })
        ));
    }

    #[test]
    fn importer_versions_pack_as_major_minor_patch() {
        assert_eq!(pack_importer_version(1, 2, 3), 1_002_003);
        assert_eq!(pack_importer_version(0, 0, 0), 0);
    }
}
