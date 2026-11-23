// SPDX-License-Identifier: MPL-2.0
//! Vorbis decoding (`A_VORBIS`) through `lewton`, a safe Rust decoder (specs/50 § 9, ADR-1869).
//!
//! The track's CodecPrivate holds the three Vorbis headers in Xiph lacing ([`crate::xiph::xiph_unlace`]):
//! identification, comment, setup. Blocks carry raw audio packets (no Ogg pages). Output is
//! interleaved `f32` at the identification header's rate and channel count, channels above two
//! reordered to the WAVE order.

use lewton::audio::{PreviousWindowRight, read_audio_packet_generic};
use lewton::header::{IdentHeader, SetupHeader, read_header_comment, read_header_ident, read_header_setup};
use lewton::samples::InterleavedSamples;

use crate::codec::AudioDecoder;
use crate::container::{Packet, TrackInfo};
use crate::error::MediaError;
use crate::xiph::{vorbis_to_wave_order, xiph_unlace};

/// The decoder's name in logs and `video.codecs`.
pub const NAME: &str = "lewton";

/// A Vorbis decoder for one track.
pub struct VorbisDecoder {
    ident: IdentHeader,
    setup: SetupHeader,
    pwr: PreviousWindowRight,
}

impl std::fmt::Debug for VorbisDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VorbisDecoder")
            .field("channels", &self.ident.audio_channels)
            .field("rate", &self.ident.audio_sample_rate)
            .finish_non_exhaustive()
    }
}

impl VorbisDecoder {
    /// Opens a decoder from the track's laced headers.
    pub fn new(track: &TrackInfo) -> Result<Self, MediaError> {
        let bad = |what: &str, e: lewton::header::HeaderReadError| {
            MediaError::invalid(format!("Vorbis {what} header: {e:?}"))
        };
        let headers = xiph_unlace(&track.codec_private)?;
        let [ident, comment, setup] = headers[..] else {
            return Err(MediaError::invalid(format!(
                "Vorbis CodecPrivate holds {} headers, expected 3",
                headers.len()
            )));
        };
        let ident = read_header_ident(ident).map_err(|e| bad("identification", e))?;
        if ident.audio_channels == 0 || ident.audio_sample_rate == 0 {
            return Err(MediaError::invalid("Vorbis stream with no channels or rate"));
        }
        if ident.audio_channels > crate::container::MAX_AUDIO_CHANNELS {
            return Err(MediaError::invalid(
                "Vorbis stream with more channels than a track may have",
            ));
        }
        read_header_comment(comment).map_err(|e| bad("comment", e))?;
        let setup = read_header_setup(
            setup,
            ident.audio_channels,
            (ident.blocksize_0, ident.blocksize_1),
        )
        .map_err(|e| bad("setup", e))?;
        Ok(Self {
            ident,
            setup,
            pwr: PreviousWindowRight::new(),
        })
    }
}

impl AudioDecoder for VorbisDecoder {
    fn decode(&mut self, packet: &Packet, out: &mut Vec<f32>) -> Result<(), MediaError> {
        let _span = tracing::trace_span!("video_audio", codec = NAME).entered();
        let decoded: InterleavedSamples<f32> =
            read_audio_packet_generic(&self.ident, &self.setup, &packet.data, &mut self.pwr)
                .map_err(|e| MediaError::DecodeFailed(format!("Vorbis decode error: {e:?}")))?;
        let start = out.len();
        out.extend(
            decoded
                .samples
                .iter()
                .map(|v| if v.is_finite() { v.clamp(-1.0, 1.0) } else { 0.0 }),
        );
        vorbis_to_wave_order(&mut out[start..], usize::from(self.ident.audio_channels));
        Ok(())
    }

    fn flush(&mut self) {
        self.pwr = PreviousWindowRight::new();
    }

    fn output_format(&self) -> Option<(u8, u32)> {
        Some((self.ident.audio_channels, self.ident.audio_sample_rate))
    }
}

#[cfg(test)]
mod tests {
    use lewton::header::{HeaderReadError, read_header_setup};

    /// LSB-first bit writer for hand-built Vorbis setup headers.
    #[derive(Default)]
    struct Bits {
        bytes: Vec<u8>,
        used: usize,
    }

    impl Bits {
        fn put(&mut self, value: u32, count: usize) {
            for i in 0..count {
                if self.used.is_multiple_of(8) {
                    self.bytes.push(0);
                }
                if (value >> i) & 1 == 1 {
                    let last = self.bytes.len() - 1;
                    self.bytes[last] |= 1 << (self.used % 8);
                }
                self.used += 1;
            }
        }
    }

    /// A setup header that stops (truncated) right after the first floor: a rejection there is
    /// `HeaderBadFormat`, anything accepted runs out of bytes instead.
    fn setup_upto_floor0(dimensions: u32, order: u32, book: u32) -> Vec<u8> {
        let mut b = Bits::default();
        b.put(5, 8);
        for c in b"vorbis" {
            b.put(u32::from(*c), 8);
        }
        b.put(0, 8); // one codebook
        b.put(0x56_4342, 24);
        b.put(dimensions, 16);
        b.put(2, 24); // two entries
        b.put(0, 1); // not ordered
        b.put(0, 1); // not sparse
        b.put(0, 5); // length 1
        b.put(0, 5); // length 1
        b.put(0, 4); // no lookup
        b.put(0, 6); // one time transform
        b.put(0, 16);
        b.put(0, 6); // one floor
        b.put(0, 16); // floor type 0
        b.put(order, 8);
        b.put(8000, 16);
        b.put(1, 16); // bark map size
        b.put(1, 6); // amplitude bits
        b.put(1, 8); // amplitude offset
        b.put(0, 4); // one book
        b.put(book, 8);
        b.bytes
    }

    fn rejected_as_bad_format(packet: &[u8]) -> bool {
        matches!(
            read_header_setup(packet, 2, (8, 11)),
            Err(HeaderReadError::HeaderBadFormat)
        )
    }

    #[test]
    fn floor0_book_index_equal_to_codebook_count_is_rejected() {
        assert!(rejected_as_bad_format(&setup_upto_floor0(1, 4, 1)));
        assert!(
            !rejected_as_bad_format(&setup_upto_floor0(1, 4, 0)),
            "control: index 0 is valid"
        );
    }

    #[test]
    fn floor0_order_below_two_is_rejected() {
        assert!(rejected_as_bad_format(&setup_upto_floor0(1, 0, 0)));
        assert!(rejected_as_bad_format(&setup_upto_floor0(1, 1, 0)));
        assert!(
            !rejected_as_bad_format(&setup_upto_floor0(1, 2, 0)),
            "control: order 2 is valid"
        );
    }

    #[test]
    fn zero_dimension_codebook_is_rejected() {
        assert!(rejected_as_bad_format(&setup_upto_floor0(0, 4, 0)));
    }
}
