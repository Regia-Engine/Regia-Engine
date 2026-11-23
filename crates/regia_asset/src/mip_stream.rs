// SPDX-License-Identifier: MPL-2.0
//! Mip streaming metadata a cooked texture carries (T2.14, ADR-1942; `docs/specs/12-assets.md` § 7.1a).
//!
//! The streamer decides from screen size what to keep resident, but only the cook has seen the
//! pixels: it knows that the top levels of a flat roughness map add nothing, and an author's
//! `.meta` can say that a texture must never stream. That lands in the KTX2 key/value data under
//! [`MIP_STREAM_KEY`] (Khronos KTX 2.0 specification § 3.11: unknown keys are skipped), so the
//! texture carries it through the pak and the frozen `CookedTexture` shape does not change. A
//! texture cooked before this has no entry and streams by the tier policy alone.

use serde::{Deserialize, Serialize};

/// The KTX2 key of the value, a postcard-encoded [`MipStreamMeta`].
pub const MIP_STREAM_KEY: &str = "regia.mipstream";

/// The value's current version. A newer value is skipped: the tier policy still works.
pub const MIP_STREAM_VERSION: u32 = 1;

/// What the cook knows about a texture's mip chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MipStreamMeta {
    /// [`MIP_STREAM_VERSION`] when written.
    pub version: u32,
    /// `false` keeps the whole chain resident (up to the tier cap): an author's `.meta` choice for
    /// a texture that must never show a low mip.
    pub stream: bool,
    /// A larger streaming floor than the tier's, texels on the largest edge (`0` = the tier's).
    pub min_resident_px: u32,
    /// The largest edge at which a level still adds visible detail: the levels above it only
    /// repeat what the next smaller level already shows (`0` = every level adds detail).
    pub detail_px: u32,
}

impl MipStreamMeta {
    /// Metadata for a texture whose largest useful edge is `detail_px`.
    #[must_use]
    pub fn new(detail_px: u32) -> Self {
        Self {
            version: MIP_STREAM_VERSION,
            stream: true,
            min_resident_px: 0,
            detail_px,
        }
    }

    /// The KTX2 value.
    #[must_use]
    pub fn to_value(&self) -> Vec<u8> {
        postcard::to_allocvec(self).unwrap_or_default()
    }

    /// Reads a KTX2 value. `None` for a value this build cannot read.
    #[must_use]
    pub fn from_value(value: &[u8]) -> Option<Self> {
        postcard::from_bytes::<Self>(value)
            .ok()
            .filter(|meta| meta.version <= MIP_STREAM_VERSION)
    }

    /// The metadata of a KTX2 file the reader accepted.
    #[must_use]
    pub fn from_reader<D: AsRef<[u8]>>(reader: &ktx2::Reader<D>) -> Option<Self> {
        reader
            .key_value_data()
            .find(|(key, _)| *key == MIP_STREAM_KEY)
            .and_then(|(_, value)| Self::from_value(value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_value_round_trips() {
        let meta = MipStreamMeta {
            min_resident_px: 512,
            ..MipStreamMeta::new(1024)
        };
        assert_eq!(MipStreamMeta::from_value(&meta.to_value()), Some(meta));
    }

    #[test]
    fn hostile_bytes_never_panic() {
        // A pak is untrusted input (ADR-538): every byte string decodes or is refused.
        let mut state = 0x9E37_79B9_u32;
        for length in 0..64 {
            for _ in 0..200 {
                let bytes: Vec<u8> = (0..length)
                    .map(|_| {
                        state = state.wrapping_mul(2_654_435_761).wrapping_add(1);
                        (state >> 24) as u8
                    })
                    .collect();
                let _ = MipStreamMeta::from_value(&bytes);
            }
        }
    }

    #[test]
    fn a_newer_or_broken_value_is_skipped() {
        let newer = MipStreamMeta {
            version: MIP_STREAM_VERSION + 1,
            ..MipStreamMeta::new(16)
        };
        assert_eq!(MipStreamMeta::from_value(&newer.to_value()), None);
        assert_eq!(MipStreamMeta::from_value(&[]), None);
        assert_eq!(MipStreamMeta::from_value(&[1]), None);
    }
}
