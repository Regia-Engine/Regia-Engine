// SPDX-License-Identifier: MPL-2.0
//! The Opus encoder of the WebM pipeline: `unsafe-libopus` (libopus 1.3.1 translated to unsafe Rust,
//! BSD-3-Clause), the library the decoder and voice chat already use (ADR-1869, ADR-1911, ADR-3369).
//!
//! One [`OpusStreamEncoder`] turns a whole interleaved `f32` signal (mono or stereo, at 8, 12, 16, 24
//! or 48 kHz) into 20 ms Opus packets with their WebM timing (RFC 7845 and the Matroska Opus
//! mapping): the `OpusHead` CodecPrivate carries the encoder's look-ahead as the pre-skip, the
//! track's `CodecDelay` is that pre-skip in nanoseconds, block `k` is stamped `k * 20 ms` (block
//! times include the codec delay), and the last block's `DiscardPadding` trims the zeros that flushed
//! the look-ahead, so a reader that honours both gets exactly the input length back. A reader that
//! ignores `DiscardPadding` (Regia's player drops the pre-skip only) gets less than one packet more.
//!
//! The encoder runs in libopus's restricted low-delay application: CELT only, never SILK or hybrid,
//! so no packet can reach the SILK concealment path that `unsafe-libopus` 0.2.0 mistranslated
//! (ADR-1911); at the default 128 kbit/s CELT is the mode libopus picks for music anyway. Variable
//! bitrate, complexity 10, no DTX.

use unsafe_libopus::{
    OPUS_APPLICATION_RESTRICTED_LOWDELAY, OPUS_GET_LOOKAHEAD_REQUEST, OPUS_OK, OPUS_SET_BITRATE_REQUEST,
    OPUS_SET_COMPLEXITY_REQUEST, OPUS_SET_DTX_REQUEST, OPUS_SET_VBR_REQUEST, OpusEncoder, opus_encode_float,
    opus_encoder_create, opus_encoder_destroy,
};

use super::EncodeError;

/// The rate Opus timing is counted in.
pub const OPUS_CLOCK: u32 = 48_000;
/// Samples per channel of one packet at 48 kHz (20 ms).
pub const PACKET_48K: u64 = 960;
/// Nanoseconds of one packet.
pub const PACKET_NS: i64 = 20_000_000;
/// `SeekPreRoll` of an Opus track: 80 ms (RFC 7845 § 4.6).
pub const SEEK_PRE_ROLL_NS: u64 = 80_000_000;
/// Largest packet libopus writes (three 1275-byte frames, RFC 6716 § 3.4) plus slack.
const MAX_PACKET: usize = 4_000;

/// One Opus packet with its WebM timing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpusPacket {
    /// Block time in nanoseconds (`k * 20 ms`, codec delay included).
    pub pts_ns: i64,
    /// `DiscardPadding` in nanoseconds (non-zero on the last packet only).
    pub discard_padding_ns: i64,
    /// The packet.
    pub data: Vec<u8>,
}

/// An Opus encoder for one mono or stereo stream.
pub struct OpusStreamEncoder {
    st: *mut OpusEncoder,
    rate: u32,
    channels: u8,
    /// Encoder look-ahead at the input rate.
    lookahead: u32,
}

// SAFETY: the encoder owns its libopus state exclusively; it is only reached through `&mut self`,
// so moving it to another thread moves the only handle.
unsafe impl Send for OpusStreamEncoder {}

impl std::fmt::Debug for OpusStreamEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpusStreamEncoder")
            .field("rate", &self.rate)
            .field("channels", &self.channels)
            .field("lookahead", &self.lookahead)
            .finish_non_exhaustive()
    }
}

impl Drop for OpusStreamEncoder {
    fn drop(&mut self) {
        // SAFETY: `st` came from `opus_encoder_create` and is destroyed once, here.
        unsafe { opus_encoder_destroy(self.st) };
    }
}

fn opus_failed(what: &str, code: i32) -> EncodeError {
    EncodeError::new("encode_failed", format!("opus {what} failed (code {code})"))
}

impl OpusStreamEncoder {
    /// An encoder for `channels` (1 or 2) at `rate` (one of [`super::AUDIO_RATES`]) and `kbps`.
    ///
    /// # Errors
    /// `encode_audio_format` for another rate or channel count; `encode_failed` when libopus refuses.
    pub fn new(rate: u32, channels: u16, kbps: u32) -> Result<Self, EncodeError> {
        if !super::AUDIO_RATES.contains(&rate) {
            return Err(EncodeError::new(
                "encode_audio_format",
                format!("sound at {rate} Hz: Opus takes 8000, 12000, 16000, 24000 or 48000 Hz"),
            ));
        }
        if !(1..=2).contains(&channels) {
            return Err(EncodeError::new(
                "encode_audio_format",
                format!("{channels} channels: the encoder takes mono or stereo"),
            ));
        }
        if !super::AUDIO_KBPS.contains(&kbps) {
            return Err(EncodeError::new(
                "encode_audio_format",
                format!("{kbps} kbit/s is outside 16..=510"),
            ));
        }
        let mut err = 0i32;
        // SAFETY: `err` is a live local read after the call; rate and channels were checked above.
        let st = unsafe {
            opus_encoder_create(
                rate as i32,
                i32::from(channels),
                OPUS_APPLICATION_RESTRICTED_LOWDELAY,
                &mut err,
            )
        };
        if st.is_null() || err != OPUS_OK {
            return Err(opus_failed("encoder create", err));
        }
        let mut enc = Self {
            st,
            rate,
            channels: channels as u8,
            lookahead: 0,
        };
        enc.set(OPUS_SET_BITRATE_REQUEST, (kbps * 1000) as i32)?;
        enc.set(OPUS_SET_VBR_REQUEST, 1)?;
        enc.set(OPUS_SET_COMPLEXITY_REQUEST, 10)?;
        enc.set(OPUS_SET_DTX_REQUEST, 0)?;
        let mut lookahead = 0i32;
        // SAFETY: `st` is live; the request writes one `i32` through the reference.
        let code =
            unsafe { unsafe_libopus::opus_encoder_ctl!(enc.st, OPUS_GET_LOOKAHEAD_REQUEST, &mut lookahead) };
        if code != OPUS_OK || lookahead < 0 {
            return Err(opus_failed("look-ahead query", code));
        }
        enc.lookahead = lookahead as u32;
        Ok(enc)
    }

    fn set(&mut self, request: i32, value: i32) -> Result<(), EncodeError> {
        // SAFETY: `st` is live and reached only through `&mut self`.
        let code = unsafe { unsafe_libopus::opus_encoder_ctl!(self.st, request, value) };
        if code == OPUS_OK {
            Ok(())
        } else {
            Err(opus_failed("setting", code))
        }
    }

    /// Channels.
    pub fn channels(&self) -> u8 {
        self.channels
    }

    /// The pre-skip in 48 kHz samples (the encoder's look-ahead).
    pub fn pre_skip(&self) -> u16 {
        (u64::from(self.lookahead) * u64::from(OPUS_CLOCK) / u64::from(self.rate)) as u16
    }

    /// The track's `CodecDelay` in nanoseconds (the pre-skip).
    pub fn codec_delay_ns(&self) -> u64 {
        (u64::from(self.pre_skip()) * 1_000_000_000).div_ceil(u64::from(OPUS_CLOCK))
    }

    /// The `OpusHead` identification header (RFC 7845 § 5.1, mapping family 0): the CodecPrivate.
    pub fn opus_head(&self) -> Vec<u8> {
        let mut h = Vec::with_capacity(19);
        h.extend_from_slice(b"OpusHead");
        h.push(1);
        h.push(self.channels);
        h.extend_from_slice(&self.pre_skip().to_le_bytes());
        h.extend_from_slice(&self.rate.to_le_bytes());
        h.extend_from_slice(&0i16.to_le_bytes());
        h.push(0);
        h
    }

    /// Encodes the whole interleaved signal `pcm` (non-finite samples become silence, the rest is
    /// clamped to `-1..=1`), padding the end with silence so the look-ahead is flushed.
    ///
    /// # Errors
    /// `encode_failed` when libopus fails on a frame.
    pub fn encode_all(&mut self, pcm: &[f32]) -> Result<Vec<OpusPacket>, EncodeError> {
        let _span = tracing::debug_span!("media_encode_opus", samples = pcm.len()).entered();
        let ch = usize::from(self.channels);
        let frame = (self.rate / 50) as usize;
        let input = (pcm.len() / ch) as u64;
        let packets = (input + u64::from(self.lookahead)).div_ceil(frame as u64);
        let mut buf = vec![0f32; frame * ch];
        let mut out = vec![0u8; MAX_PACKET];
        let mut result = Vec::with_capacity(packets as usize);
        for k in 0..packets {
            let start = (k as usize * frame * ch).min(pcm.len());
            let end = (start + frame * ch).min(pcm.len());
            buf.fill(0.0);
            for (d, s) in buf.iter_mut().zip(&pcm[start..end]) {
                *d = if s.is_finite() { s.clamp(-1.0, 1.0) } else { 0.0 };
            }
            // SAFETY: `buf` holds `frame * channels` samples (the frame size passed), `out` has
            // `MAX_PACKET` bytes and libopus writes at most that many; `st` is live.
            let n = unsafe {
                opus_encode_float(
                    self.st,
                    buf.as_ptr(),
                    frame as i32,
                    out.as_mut_ptr(),
                    MAX_PACKET as i32,
                )
            };
            let n = usize::try_from(n).map_err(|_| opus_failed("encode", n))?;
            result.push(OpusPacket {
                pts_ns: k as i64 * PACKET_NS,
                discard_padding_ns: 0,
                data: out[..n].to_vec(),
            });
        }
        // Decoded length at 48 kHz is packets * 960 - pre-skip; the input is input * 48000 / rate.
        let input_48k = input * u64::from(OPUS_CLOCK) / u64::from(self.rate);
        let decoded = (packets * PACKET_48K).saturating_sub(u64::from(self.pre_skip()));
        let padding = decoded.saturating_sub(input_48k);
        if let Some(last) = result.last_mut()
            && padding > 0
        {
            last.discard_padding_ns = (padding * 1_000_000_000 / u64::from(OPUS_CLOCK)) as i64;
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_timing_and_refusals() {
        let e = OpusStreamEncoder::new(48_000, 2, 128).expect("encoder");
        let head = e.opus_head();
        assert_eq!(&head[..8], b"OpusHead");
        assert_eq!((head.len(), head[9]), (19, 2));
        assert_eq!(u16::from_le_bytes([head[10], head[11]]), e.pre_skip());
        // Restricted low delay: 2.5 ms of look-ahead.
        assert_eq!(e.pre_skip(), 120);
        assert_eq!(e.codec_delay_ns(), 2_500_000);
        let code = |r, c, k| OpusStreamEncoder::new(r, c, k).err().map(|e| e.code);
        assert_eq!(code(44_100, 2, 128), Some("encode_audio_format"));
        assert_eq!(code(48_000, 6, 128), Some("encode_audio_format"));
        assert_eq!(code(48_000, 2, 4), Some("encode_audio_format"));
    }

    #[test]
    fn packets_cover_the_input_and_the_padding_trims_it_exactly() {
        let mut e = OpusStreamEncoder::new(48_000, 1, 96).expect("encoder");
        let n = 48_000 + 123;
        let pcm: Vec<f32> = (0..n).map(|i| (i as f32 * 0.05).sin() * 0.5).collect();
        let packets = e.encode_all(&pcm).expect("encode");
        let decoded = packets.len() as u64 * PACKET_48K - u64::from(e.pre_skip());
        assert!(decoded >= n as u64 && decoded < n as u64 + PACKET_48K);
        let last = packets.last().expect("packets");
        let trimmed = decoded as i64 - (last.discard_padding_ns * 48_000).div_euclid(1_000_000_000);
        assert!((trimmed - n as i64).abs() <= 1, "{trimmed} vs {n}");
        assert!(
            packets
                .iter()
                .enumerate()
                .all(|(k, p)| p.pts_ns == k as i64 * PACKET_NS)
        );
        assert!(
            packets[..packets.len() - 1]
                .iter()
                .all(|p| p.discard_padding_ns == 0)
        );
        // Silence and garbage encode too.
        let packets = e
            .encode_all(&[f32::NAN, f32::INFINITY, 0.0, 0.0])
            .expect("silence");
        assert!(!packets.is_empty() && packets.iter().all(|p| !p.data.is_empty()));
    }
}
