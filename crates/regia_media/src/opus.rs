// SPDX-License-Identifier: MPL-2.0
//! Opus decoding (`A_OPUS`) through `unsafe-libopus`, libopus 1.3.1 translated to unsafe Rust
//! (ADR-1593, ADR-1869).
//!
//! The track's CodecPrivate is the `OpusHead` identification header (RFC 7845 § 5.1): channel count,
//! pre-skip, output gain and the channel mapping. Every mapping family goes through the multistream
//! decoder (family 0 is one stream). Output is interleaved `f32` at 48 kHz, the rate Opus always codes
//! at; channels above two are reordered from the Vorbis order to the WAVE order the mixer expects
//! ([`crate::xiph::vorbis_to_wave_order`]).
//!
//! Pre-skip is dropped at the start of the stream (also after a seek back to the start). After a seek
//! elsewhere the decoder starts cold, so the first 80 ms may differ from a straight decode (RFC 7845
//! § 4.6 pre-roll); the player's audio is tolerance-compared anyway (specs/50 § 7).
//!
//! **Packet gate (ADR-1911, ADR-2306).** `unsafe-libopus` 0.2.0's SILK packet-loss concealment pins the
//! output at full scale and leaves garbage in the decoder's memory. libopus conceals in two cases a file
//! can trigger: a frame of 0 or 1 bytes (a 1-2 byte code-0 packet) is treated as lost, and a CELT-only
//! frame right after a SILK or hybrid one conceals a SILK transition first. [`gate_packet`] walks every
//! stream of a packet (RFC 6716 § 3.2 and Appendix B) before the decoder sees it: a packet with a frame
//! of one byte or less is refused. When a stream switches from SILK or hybrid to CELT-only, libopus
//! conceals the SILK transition unless the previous frame carried the encoder's CELT redundancy (a
//! legitimate encoder sends it); if the transition crossfade then shows the concealment's full-scale
//! output, the decoder restarts cold and decodes the packet again, so its garbage never reaches the
//! output or the next packets.

use crate::codec::AudioDecoder;
use crate::container::{Packet, TrackInfo};
use crate::error::MediaError;
use crate::xiph::vorbis_to_wave_order;

/// The rate every Opus stream decodes at here.
pub const OPUS_RATE: u32 = 48_000;
/// 120 ms at 48 kHz: the longest Opus packet, per channel.
const MAX_FRAME: usize = 5_760;

/// Samples per channel of libopus's mode-transition crossfade (2.5 ms at 48 kHz).
const TRANSITION_SAMPLES: usize = 120;
/// A raw decoded sample at or above this in the transition crossfade is the broken concealment's
/// full-scale output (see the module docs), not audio.
const CONCEALMENT_BLAST: f32 = 0.99;

/// The decoder's name in logs and `video.codecs`.
pub const NAME: &str = "libopus";

/// The parsed `OpusHead`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpusHead {
    /// Output channels (1..=8, what the container allows).
    pub channels: u8,
    /// Samples (at 48 kHz) to drop at the start of the stream.
    pub pre_skip: u16,
    /// Original input rate (informational).
    pub input_rate: u32,
    /// Output gain in Q7.8 dB.
    pub output_gain: i16,
    /// Channel mapping family (0: mono/stereo, 1: Vorbis order up to 8, 255: undefined order).
    pub mapping_family: u8,
    /// Coded streams.
    pub streams: u8,
    /// Streams that carry two channels.
    pub coupled: u8,
    /// Output channel to decoded channel (255: silence).
    pub mapping: Vec<u8>,
}

impl OpusHead {
    /// Parses an `OpusHead` (RFC 7845 § 5.1).
    pub fn parse(b: &[u8]) -> Result<Self, MediaError> {
        let bad = |why: &str| MediaError::invalid(format!("Opus header {why}"));
        if b.len() < 19 || &b[..8] != b"OpusHead" {
            return Err(bad("missing (CodecPrivate must hold OpusHead)"));
        }
        if b[8] >> 4 != 0 {
            return Err(bad("of an unsupported major version"));
        }
        let channels = b[9];
        if channels == 0 {
            return Err(bad("with zero channels"));
        }
        if channels > crate::container::MAX_AUDIO_CHANNELS {
            return Err(bad("with more channels than a track may have"));
        }
        let pre_skip = u16::from_le_bytes([b[10], b[11]]);
        let input_rate = u32::from_le_bytes([b[12], b[13], b[14], b[15]]);
        let output_gain = i16::from_le_bytes([b[16], b[17]]);
        let mapping_family = b[18];
        let (streams, coupled, mapping) = if mapping_family == 0 {
            if channels > 2 {
                return Err(bad("family 0 with more than two channels"));
            }
            (1, channels - 1, (0..channels).collect())
        } else {
            let table = b
                .get(19..21 + usize::from(channels))
                .ok_or_else(|| bad("truncated"))?;
            let (streams, coupled) = (table[0], table[1]);
            if streams == 0 || coupled > streams || usize::from(streams) + usize::from(coupled) > 255 {
                return Err(bad("with a bad stream count"));
            }
            let mapping = table[2..].to_vec();
            let decoded = streams + coupled;
            if mapping.iter().any(|&m| m != 255 && m >= decoded) {
                return Err(bad("with a mapping past the decoded channels"));
            }
            (streams, coupled, mapping)
        };
        Ok(Self {
            channels,
            pre_skip,
            input_rate,
            output_gain,
            mapping_family,
            streams,
            coupled,
            mapping,
        })
    }

    /// Linear output gain.
    pub fn gain(&self) -> f32 {
        10f32.powf(f32::from(self.output_gain) / (20.0 * 256.0))
    }
}

/// The coding mode of an Opus frame, from its TOC byte (RFC 6716 § 3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpusMode {
    /// SILK only (configurations 0-11, speech).
    Silk,
    /// SILK plus CELT (configurations 12-15).
    Hybrid,
    /// CELT only (configurations 16-31, music and low delay).
    Celt,
}

impl OpusMode {
    /// The mode of a TOC byte.
    #[must_use]
    pub fn of_toc(toc: u8) -> Self {
        match toc >> 3 {
            0..=11 => Self::Silk,
            12..=15 => Self::Hybrid,
            _ => Self::Celt,
        }
    }

    /// Whether the decoder's SILK layer holds state (so a loss would conceal through SILK).
    #[must_use]
    pub fn uses_silk(self) -> bool {
        matches!(self, Self::Silk | Self::Hybrid)
    }
}

/// Samples per frame at 48 kHz for a TOC byte (RFC 6716 § 3.1, Table 2).
fn frame_samples(toc: u8) -> usize {
    let config = toc >> 3;
    match config {
        0..=11 => [480, 960, 1920, 2880][usize::from(config & 3)],
        12..=15 => [480, 960][usize::from(config & 1)],
        _ => [120, 240, 480, 960][usize::from(config & 3)],
    }
}

/// Reads a frame length (RFC 6716 § 3.2.1): one byte below 252, else two bytes.
fn frame_length(b: &[u8], at: &mut usize) -> Result<usize, &'static str> {
    let first = *b.get(*at).ok_or("a frame length past the end")?;
    if first < 252 {
        *at += 1;
        return Ok(usize::from(first));
    }
    let second = *b.get(*at + 1).ok_or("a frame length past the end")?;
    *at += 2;
    Ok(usize::from(first) + 4 * usize::from(second))
}

/// Walks one stream's packet starting at `b[0]`. `self_delimited` is the framing of every stream
/// but the last in a multistream packet (RFC 6716 Appendix B). Returns the stream's mode, its
/// smallest frame and the bytes it took.
fn walk_stream(b: &[u8], self_delimited: bool) -> Result<(OpusMode, usize, usize), &'static str> {
    let toc = *b.first().ok_or("an empty stream")?;
    let mode = OpusMode::of_toc(toc);
    let mut at = 1;
    let mut frames: Vec<usize> = Vec::with_capacity(2);
    let mut padding = 0usize;
    let rest = |at: usize, padding: usize| -> Result<usize, &'static str> {
        b.len()
            .checked_sub(at)
            .and_then(|r| r.checked_sub(padding))
            .ok_or("lengths past the end")
    };
    match toc & 0x03 {
        0 => {
            let n = if self_delimited {
                frame_length(b, &mut at)?
            } else {
                rest(at, 0)?
            };
            frames.push(n);
        }
        1 => {
            let n = if self_delimited {
                frame_length(b, &mut at)?
            } else {
                let r = rest(at, 0)?;
                if !r.is_multiple_of(2) {
                    return Err("two equal frames of an odd total");
                }
                r / 2
            };
            frames.extend([n, n]);
        }
        2 => {
            let first = frame_length(b, &mut at)?;
            let second = if self_delimited {
                frame_length(b, &mut at)?
            } else {
                rest(at, 0)?
                    .checked_sub(first)
                    .ok_or("a first frame past the end")?
            };
            frames.extend([first, second]);
        }
        _ => {
            let count = *b.get(at).ok_or("no frame count")?;
            at += 1;
            let m = usize::from(count & 0x3f);
            if m == 0 || m * frame_samples(toc) > MAX_FRAME {
                return Err("a frame count of zero or over 120 ms");
            }
            if count & 0x40 != 0 {
                loop {
                    let p = *b.get(at).ok_or("padding past the end")?;
                    at += 1;
                    padding += if p == 255 { 254 } else { usize::from(p) };
                    if p != 255 {
                        break;
                    }
                }
            }
            if count & 0x80 != 0 {
                let coded = if self_delimited { m } else { m - 1 };
                for _ in 0..coded {
                    frames.push(frame_length(b, &mut at)?);
                }
                if !self_delimited {
                    let used: usize = frames.iter().sum();
                    frames.push(
                        rest(at, padding)?
                            .checked_sub(used)
                            .ok_or("frames past the end")?,
                    );
                }
            } else {
                let n = if self_delimited {
                    frame_length(b, &mut at)?
                } else {
                    let r = rest(at, padding)?;
                    if !r.is_multiple_of(m) {
                        return Err("equal frames that do not divide the packet");
                    }
                    r / m
                };
                frames.extend(std::iter::repeat_n(n, m));
            }
        }
    }
    let total = at + frames.iter().sum::<usize>() + padding;
    if total > b.len() {
        return Err("frames past the end");
    }
    if !self_delimited && total != b.len() {
        return Err("bytes left after the last stream");
    }
    let smallest = frames.iter().copied().min().unwrap_or(0);
    Ok((mode, smallest, total))
}

/// Checks one (multistream) Opus packet of a track with `streams` coded streams before the decoder
/// sees it, and returns each stream's mode in order.
///
/// Refused: a packet that does not parse (RFC 6716 § 3.2, Appendix B for every stream but the
/// last), and a packet with a frame of one byte or less, which libopus would conceal as lost
/// (see the module docs: the library's SILK concealment is broken).
///
/// # Errors
/// [`MediaError::DecodeFailed`] naming the problem.
pub fn gate_packet(data: &[u8], streams: u8) -> Result<Vec<OpusMode>, MediaError> {
    let refuse = |why: &str| MediaError::DecodeFailed(format!("Opus packet refused: {why}"));
    if data.is_empty() {
        // libopus treats an empty packet as a lost one and conceals a whole 120 ms frame.
        return Err(refuse("empty"));
    }
    let streams = usize::from(streams.max(1));
    let mut modes = Vec::with_capacity(streams);
    let mut at = 0;
    for s in 0..streams {
        let last = s + 1 == streams;
        let (mode, smallest, used) = walk_stream(&data[at..], !last).map_err(refuse)?;
        if smallest <= 1 {
            return Err(refuse(
                "a frame of one byte or less (libopus would conceal it as lost)",
            ));
        }
        modes.push(mode);
        at += used;
    }
    Ok(modes)
}

/// Owns one libopus multistream decoder.
struct Raw(*mut unsafe_libopus::OpusMSDecoder);

impl Raw {
    fn open(head: &OpusHead) -> Result<Self, MediaError> {
        let mut err = 0i32;
        // SAFETY: `mapping` holds `channels` bytes (checked by `OpusHead::parse`), which libopus reads
        // during the call only; `err` is a live local.
        let st = unsafe {
            unsafe_libopus::opus_multistream_decoder_create(
                OPUS_RATE as i32,
                i32::from(head.channels),
                i32::from(head.streams),
                i32::from(head.coupled),
                head.mapping.as_ptr(),
                &mut err,
            )
        };
        if st.is_null() || err != 0 {
            return Err(MediaError::invalid(format!(
                "the Opus decoder refused the stream layout (error {err})"
            )));
        }
        Ok(Self(st))
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        // SAFETY: the pointer came from `opus_multistream_decoder_create` and is destroyed once, here.
        unsafe { unsafe_libopus::opus_multistream_decoder_destroy(self.0) };
    }
}

/// An Opus decoder for one track.
pub struct OpusDecoder {
    head: OpusHead,
    raw: Raw,
    pcm: Vec<f32>,
    gain: f32,
    /// Samples (per channel) still to drop.
    skip: usize,
    /// The next packet decides the pre-skip (set at open and by every flush).
    flushed: bool,
    /// Each stream's mode in the last packet the decoder took (`None`: a cold decoder).
    modes: Vec<Option<OpusMode>>,
}

// SAFETY: the libopus state is plain memory owned by this value alone (no thread affinity, no shared
// globals), and it is only reached through `&mut self`.
unsafe impl Send for OpusDecoder {}

impl std::fmt::Debug for OpusDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpusDecoder")
            .field("head", &self.head)
            .finish_non_exhaustive()
    }
}

impl OpusDecoder {
    /// Opens a decoder from the track's `OpusHead`.
    pub fn new(track: &TrackInfo) -> Result<Self, MediaError> {
        let head = OpusHead::parse(&track.codec_private)?;
        let raw = Raw::open(&head)?;
        Ok(Self {
            gain: head.gain(),
            skip: 0,
            pcm: vec![0.0; MAX_FRAME * usize::from(head.channels)],
            raw,
            modes: vec![None; usize::from(head.streams)],
            head,
            flushed: true,
        })
    }

    /// Opens a decoder from an already parsed `OpusHead` (the fuzz entry point and tests use it;
    /// a track opens through [`OpusDecoder::new`]).
    pub fn from_head(head: OpusHead) -> Result<Self, MediaError> {
        let raw = Raw::open(&head)?;
        Ok(Self {
            gain: head.gain(),
            skip: 0,
            pcm: vec![0.0; MAX_FRAME * usize::from(head.channels)],
            raw,
            modes: vec![None; usize::from(head.streams)],
            head,
            flushed: true,
        })
    }

    /// Restarts the libopus state cold (the pre-skip bookkeeping is unchanged).
    fn restart(&mut self) {
        match Raw::open(&self.head) {
            Ok(raw) => self.raw = raw,
            Err(e) => tracing::warn!("Opus decoder reset failed, keeping the old state: {e}"),
        }
        self.modes.fill(None);
    }

    /// One libopus decode of `data` into `pcm`; returns the samples per channel. A libopus error
    /// restarts the state (the streams before the failing one may have moved on).
    fn decode_raw(&mut self, data: &[u8]) -> Result<usize, MediaError> {
        let len = i32::try_from(data.len())
            .map_err(|_| MediaError::DecodeFailed("Opus packet too large".into()))?;
        // SAFETY: `pcm` holds `MAX_FRAME * channels` floats, the frame size passed; the packet slice is
        // valid for `len` bytes during the call.
        let n = unsafe {
            unsafe_libopus::opus_multistream_decode_float(
                self.raw.0,
                data.as_ptr(),
                len,
                self.pcm.as_mut_ptr(),
                MAX_FRAME as i32,
                0,
            )
        };
        if n < 0 {
            self.restart();
            return Err(MediaError::DecodeFailed(format!("Opus decode error {n}")));
        }
        Ok((n as usize).min(MAX_FRAME))
    }

    /// Whether the transition crossfade (the first 2.5 ms) of the last decode shows the broken SILK
    /// concealment of `unsafe-libopus` 0.2.0: it saturates every sample to +1.0, and the crossfade
    /// starts at the concealed signal, so its first samples sit at full scale.
    fn concealment_blast(&self, frames: usize) -> bool {
        let channels = usize::from(self.head.channels);
        let window = TRANSITION_SAMPLES.min(frames) * channels;
        self.pcm[..window].iter().any(|v| *v >= CONCEALMENT_BLAST)
    }

    /// The parsed header.
    pub fn head(&self) -> &OpusHead {
        &self.head
    }
}

/// The frame lengths of a single-stream Opus packet (RFC 6716 § 3.2), or `None` when it is
/// malformed (left to the library, which refuses it).
fn frame_lengths(packet: &[u8]) -> Option<Vec<usize>> {
    let (&toc, rest) = packet.split_first()?;
    match toc & 0x03 {
        0 => Some(vec![rest.len()]),
        1 => rest.len().is_multiple_of(2).then(|| vec![rest.len() / 2; 2]),
        2 => {
            let (first, used) = frame_length_and_size(rest)?;
            let second = rest.len().checked_sub(used)?.checked_sub(first)?;
            Some(vec![first, second])
        }
        _ => {
            let (&count_byte, mut rest) = rest.split_first()?;
            let count = usize::from(count_byte & 0x3F);
            if count == 0 {
                return None;
            }
            if count_byte & 0x40 != 0 {
                let mut padding = 0usize;
                loop {
                    let (&b, tail) = rest.split_first()?;
                    rest = tail;
                    padding += if b == 255 { 254 } else { usize::from(b) };
                    if b != 255 {
                        break;
                    }
                }
                rest = rest.get(..rest.len().checked_sub(padding)?)?;
            }
            if count_byte & 0x80 == 0 {
                return rest
                    .len()
                    .is_multiple_of(count)
                    .then(|| vec![rest.len() / count; count]);
            }
            let mut lengths = Vec::with_capacity(count);
            let mut used = 0usize;
            for _ in 1..count {
                let (len, n) = frame_length_and_size(rest.get(used..)?)?;
                used += n;
                lengths.push(len);
            }
            let last = rest
                .len()
                .checked_sub(used)?
                .checked_sub(lengths.iter().sum::<usize>())?;
            lengths.push(last);
            Some(lengths)
        }
    }
}

/// One frame length (one or two bytes, RFC 6716 § 3.2.1) and the bytes it took.
fn frame_length_and_size(b: &[u8]) -> Option<(usize, usize)> {
    let first = usize::from(*b.first()?);
    if first < 252 {
        Some((first, 1))
    } else {
        Some((first + 4 * usize::from(*b.get(1)?), 2))
    }
}

/// `Some(samples per channel)` when libopus would conceal `packet` instead of decoding it: a packet
/// of one or two bytes (no audio in it), or, for a single-stream track, one with a zero-length frame.
pub(crate) fn concealed_samples(packet: &[u8], streams: u8) -> Result<Option<usize>, MediaError> {
    let Some(&toc) = packet.first() else {
        return Ok(None);
    };
    let lengths = if streams == 1 { frame_lengths(packet) } else { None };
    let conceals = packet.len() <= 2 || lengths.as_ref().is_some_and(|l| l.contains(&0));
    if !conceals {
        return Ok(None);
    }
    let frames = match (&lengths, toc & 0x03) {
        (Some(l), _) => l.len(),
        (None, 0) => 1,
        (None, 1 | 2) => 2,
        (None, _) => usize::from(packet.get(1).map_or(0, |b| b & 0x3F)),
    };
    let samples = frames * frame_samples(toc);
    if samples == 0 || samples > MAX_FRAME {
        return Err(MediaError::DecodeFailed(format!(
            "Opus packet of {frames} frames is not a valid duration"
        )));
    }
    Ok(Some(samples))
}

/// Pre-skip samples to drop for a decode that restarts at `pts_ns`: the whole pre-skip when the packet is
/// at the real start of the stream (within the pre-skip span), none after a seek or phase offset that
/// lands mid-stream (RFC 7845 § 4.2 applies pre-skip to the stream start only).
fn skip_at(pts_ns: i64, pre_skip: u16) -> usize {
    let span_ns = i64::from(pre_skip) * 1_000_000_000 / i64::from(OPUS_RATE);
    if pts_ns <= span_ns {
        usize::from(pre_skip)
    } else {
        0
    }
}

impl AudioDecoder for OpusDecoder {
    fn decode(&mut self, packet: &Packet, out: &mut Vec<f32>) -> Result<(), MediaError> {
        let _span = tracing::trace_span!("video_audio", codec = NAME).entered();
        if self.flushed {
            self.flushed = false;
            self.skip = skip_at(packet.pts_ns, self.head.pre_skip);
        }
        if let Some(samples) = concealed_samples(&packet.data, self.head.streams)? {
            // The library would run its packet-loss concealment here, which `unsafe-libopus` 0.2.0
            // mistranslated (it pins SILK output at full scale and keeps it in the decoder state,
            // ADR-1911). The packet is silence of its duration instead; the decoder never sees it.
            let channels = usize::from(self.head.channels);
            let drop = self.skip.min(samples);
            self.skip -= drop;
            out.resize(out.len() + (samples - drop) * channels, 0.0);
            return Ok(());
        }
        let modes = gate_packet(&packet.data, self.head.streams)?;
        // A stream going from SILK or hybrid to CELT-only: unless the previous frame carried the
        // encoder's CELT redundancy (not visible from outside the decoder), libopus conceals a
        // SILK transition first, the broken path. Its output fills the first 2.5 ms crossfade.
        let transition = modes
            .iter()
            .zip(&self.modes)
            .any(|(now, before)| *now == OpusMode::Celt && before.is_some_and(OpusMode::uses_silk));
        let mut frames = self.decode_raw(&packet.data)?;
        if transition && self.concealment_blast(frames) {
            // The concealment ran: drop its garbage with the whole state and decode cold.
            self.restart();
            frames = self.decode_raw(&packet.data)?;
        }
        for (slot, mode) in self.modes.iter_mut().zip(modes) {
            *slot = Some(mode);
        }
        let channels = usize::from(self.head.channels);
        let drop = self.skip.min(frames);
        self.skip -= drop;
        let start = out.len();
        out.extend(self.pcm[drop * channels..frames * channels].iter().map(|s| {
            let v = s * self.gain;
            if v.is_finite() { v.clamp(-1.0, 1.0) } else { 0.0 }
        }));
        if self.head.mapping_family == 1 {
            vorbis_to_wave_order(&mut out[start..], channels);
        }
        Ok(())
    }

    fn flush(&mut self) {
        self.restart();
        self.skip = 0;
        self.flushed = true;
    }

    fn output_format(&self) -> Option<(u8, u32)> {
        Some((self.head.channels, OPUS_RATE))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(channels: u8, family: u8, extra: &[u8]) -> Vec<u8> {
        let mut b = b"OpusHead".to_vec();
        b.extend_from_slice(&[1, channels, 0x38, 0x01, 0x80, 0xbb, 0, 0, 0, 0, family]);
        b.extend_from_slice(extra);
        b
    }

    #[test]
    fn opus_head_past_eight_channels_is_refused() {
        // A 19-byte header would otherwise size a ring of 255 channels.
        let mapping: Vec<u8> = (0..9).collect();
        let mut table = vec![9, 0];
        table.extend(&mapping);
        assert!(OpusHead::parse(&head(9, 255, &table)).is_err());
        let mut table = vec![255, 0];
        table.extend((0..=254u8).collect::<Vec<_>>());
        assert!(OpusHead::parse(&head(255, 255, &table)).is_err());
    }

    #[test]
    fn opus_head_family_0_and_1() {
        let h = OpusHead::parse(&head(2, 0, &[])).expect("stereo head");
        assert_eq!(
            (h.channels, h.pre_skip, h.input_rate, h.streams, h.coupled),
            (2, 312, 48_000, 1, 1)
        );
        assert_eq!(h.mapping, vec![0, 1]);
        assert!((h.gain() - 1.0).abs() < 1e-6);
        let h = OpusHead::parse(&head(6, 1, &[4, 2, 0, 4, 1, 2, 3, 5])).expect("5.1 head");
        assert_eq!((h.streams, h.coupled, h.mapping.len()), (4, 2, 6));
    }

    #[test]
    fn opus_head_rejects_bad_layouts() {
        assert!(OpusHead::parse(b"OpusHea").is_err());
        assert!(OpusHead::parse(&head(0, 0, &[])).is_err());
        assert!(OpusHead::parse(&head(3, 0, &[])).is_err());
        assert!(OpusHead::parse(&head(6, 1, &[4, 2, 0])).is_err());
        assert!(OpusHead::parse(&head(2, 1, &[1, 2, 0, 1])).is_err());
        assert!(OpusHead::parse(&head(2, 1, &[1, 0, 0, 7])).is_err());
        let mut v2 = head(1, 0, &[]);
        v2[8] = 0x10;
        assert!(OpusHead::parse(&v2).is_err());
    }

    #[test]
    fn pre_skip_applies_only_at_the_stream_start() {
        // 312 samples is 6.5 ms.
        assert_eq!(skip_at(0, 312), 312);
        assert_eq!(skip_at(-5_000_000, 312), 312);
        assert_eq!(skip_at(1_000_000_000, 312), 0);
        assert_eq!(skip_at(20_000_000, 312), 0);
        assert_eq!(skip_at(5, 0), 0);
    }

    #[test]
    fn flush_clears_the_pending_skip_and_the_next_packet_decides() {
        let h = OpusHead::parse(&head(2, 0, &[])).expect("head");
        let raw = Raw::open(&h).expect("libopus state");
        let mut d = OpusDecoder {
            gain: h.gain(),
            skip: 0,
            pcm: vec![0.0; MAX_FRAME * 2],
            raw,
            modes: vec![None; usize::from(h.streams)],
            head: h,
            flushed: true,
        };
        assert!(d.flushed, "the first packet decides at open");
        d.flushed = false;
        d.skip = 100;
        d.flush();
        assert_eq!((d.skip, d.flushed), (0, true));
    }

    /// Mono 20 ms SILK packets of a 0.3-amplitude voiced tone, encoded at 48 kHz.
    fn silk_packets(n: usize) -> Vec<Vec<u8>> {
        use unsafe_libopus::{
            OPUS_APPLICATION_VOIP, OPUS_BANDWIDTH_WIDEBAND, OPUS_SET_BITRATE_REQUEST,
            OPUS_SET_MAX_BANDWIDTH_REQUEST, OPUS_SET_SIGNAL_REQUEST, OPUS_SIGNAL_VOICE, opus_encode_float,
            opus_encoder_create, opus_encoder_ctl, opus_encoder_destroy,
        };
        let mut err = 0i32;
        // SAFETY: `err` is a live local read after the call.
        let enc = unsafe { opus_encoder_create(48_000, 1, OPUS_APPLICATION_VOIP, &mut err) };
        assert!(!enc.is_null() && err == 0, "encoder: {err}");
        // SAFETY: `enc` is the live encoder created above.
        unsafe {
            opus_encoder_ctl!(enc, OPUS_SET_BITRATE_REQUEST, 16_000);
            opus_encoder_ctl!(enc, OPUS_SET_SIGNAL_REQUEST, OPUS_SIGNAL_VOICE);
            // Wideband at most: SILK only (hybrid needs super-wideband).
            opus_encoder_ctl!(enc, OPUS_SET_MAX_BANDWIDTH_REQUEST, OPUS_BANDWIDTH_WIDEBAND);
        }
        let frame = 960;
        let mut packets = Vec::new();
        for k in 0..n {
            let pcm: Vec<f32> = (0..frame)
                .map(|i| {
                    let t = (k * frame + i) as f32 / 48_000.0;
                    0.3 * (std::f32::consts::TAU * 220.0 * t).sin()
                        * (1.0 + 0.3 * (std::f32::consts::TAU * 3.0 * t).sin())
                        / 1.3
                })
                .collect();
            let mut buf = vec![0u8; 1500];
            // SAFETY: `pcm` holds one frame, `buf` the capacity passed.
            let len = unsafe {
                opus_encode_float(
                    enc,
                    pcm.as_ptr(),
                    frame as i32,
                    buf.as_mut_ptr(),
                    buf.len() as i32,
                )
            };
            assert!(len > 2, "encode: {len}");
            buf.truncate(len as usize);
            packets.push(buf);
        }
        // SAFETY: created above, destroyed once.
        unsafe { opus_encoder_destroy(enc) };
        packets
    }

    fn mono_decoder() -> OpusDecoder {
        let mut b = head(1, 0, &[]);
        b[10..12].copy_from_slice(&0u16.to_le_bytes());
        let h = OpusHead::parse(&b).expect("head");
        let raw = Raw::open(&h).expect("libopus state");
        OpusDecoder {
            gain: h.gain(),
            skip: 0,
            pcm: vec![0.0; MAX_FRAME],
            raw,
            modes: vec![None; usize::from(h.streams)],
            head: h,
            flushed: true,
        }
    }

    fn packet_at(data: Vec<u8>, k: usize) -> Packet {
        Packet {
            track: 1,
            pts_ns: k as i64 * 20_000_000,
            duration_ns: None,
            keyframe: true,
            data,
        }
    }

    /// Audit v034 cook M1: a packet the library would conceal (one or two bytes, or a zero-length
    /// frame) never reaches `unsafe-libopus` 0.2.0's broken SILK concealment, which pins the output
    /// at full scale (ADR-1911). It decodes as silence of the packet's duration.
    #[test]
    fn packets_the_library_would_conceal_decode_as_silence() {
        let good = silk_packets(30);
        assert!(
            good[0][0] >> 3 < 12,
            "the fixture is SILK-only: TOC {:#x}",
            good[0][0]
        );
        let toc = good[10][0] & !0x03;
        let empty_frame = {
            // Code 2: two frames, the first zero bytes long, the second a real frame.
            let mut p = vec![toc | 0x02, 0x00];
            p.extend_from_slice(&good[10][1..]);
            p
        };
        for short in [vec![toc], vec![toc, 0x00], vec![toc | 0x01], empty_frame] {
            let mut d = mono_decoder();
            let mut out = Vec::new();
            for (k, p) in good[..10].iter().enumerate() {
                d.decode(&packet_at(p.clone(), k), &mut out).expect("decodes");
            }
            let before = out.len();
            d.decode(&packet_at(short.clone(), 10), &mut out)
                .expect("a concealing packet is silence, not an error");
            let silent = &out[before..];
            assert!(!silent.is_empty(), "{short:?}: the packet's duration is kept");
            assert!(
                silent.len().is_multiple_of(960),
                "{short:?}: {} samples",
                silent.len()
            );
            let after = out.len();
            for (k, p) in good[11..].iter().enumerate() {
                d.decode(&packet_at(p.clone(), 11 + k), &mut out)
                    .expect("decodes");
            }
            let peak = out[before..].iter().fold(0.0f32, |m, s| m.max(s.abs()));
            assert!(peak < 0.9, "{short:?}: output pinned near full scale ({peak})");
            assert!(out.len() > after, "audio resumes");
        }
    }

    #[test]
    fn output_gain_is_q7_8_db() {
        let mut b = head(1, 0, &[]);
        b[16..18].copy_from_slice(&(6i16 * 256).to_le_bytes());
        let g = OpusHead::parse(&b).expect("head").gain();
        assert!((g - 1.995_262).abs() < 1e-4, "{g}");
    }

    // ---- packet gate (ADR-1911, ADR-2306) ----

    /// TOC bytes: SILK WB 20 ms, hybrid FB 20 ms, CELT FB 20 ms; code 0 (one frame).
    const SILK: u8 = 9 << 3;
    const HYBRID: u8 = 15 << 3;
    const CELT: u8 = 31 << 3;

    #[test]
    fn gate_refuses_frames_libopus_would_conceal() {
        for bad in [
            &[][..],
            &[SILK][..],
            &[SILK, 0x55][..],
            &[CELT][..],
            &[CELT, 0][..],
        ] {
            assert!(gate_packet(bad, 1).is_err(), "{bad:?}");
        }
        assert_eq!(
            gate_packet(&[SILK, 1, 2], 1).expect("3 bytes"),
            vec![OpusMode::Silk]
        );
        assert_eq!(
            gate_packet(&[CELT, 1, 2], 1).expect("3 bytes"),
            vec![OpusMode::Celt]
        );
        assert_eq!(
            gate_packet(&[HYBRID, 1, 2], 1).expect("3 bytes"),
            vec![OpusMode::Hybrid]
        );
        // Code 1: two equal frames; an odd total or 1-byte halves are refused.
        assert!(gate_packet(&[SILK | 1, 1, 2, 3, 4], 1).is_ok());
        assert!(gate_packet(&[SILK | 1, 1, 2, 3], 1).is_err());
        assert!(gate_packet(&[SILK | 1, 1, 2], 1).is_err());
        // Code 2: a coded first length; a 1-byte or 0-byte frame either side is refused.
        assert!(gate_packet(&[SILK | 2, 2, 9, 9, 7, 7, 7], 1).is_ok());
        assert!(gate_packet(&[SILK | 2, 1, 9, 7, 7, 7], 1).is_err());
        assert!(gate_packet(&[SILK | 2, 4, 9, 9, 9, 9, 7], 1).is_err());
        assert!(gate_packet(&[SILK | 2, 0, 7, 7], 1).is_err());
        assert!(
            gate_packet(&[SILK | 2, 9, 7, 7], 1).is_err(),
            "first frame past the end"
        );
    }

    #[test]
    fn gate_walks_code_3_padding_and_vbr() {
        // CBR, 3 frames of 2 bytes, 2 bytes of padding.
        let cbr = [CELT | 3, 0x40 | 3, 2, 1, 1, 2, 2, 3, 3, 0, 0];
        assert_eq!(gate_packet(&cbr, 1).expect("cbr"), vec![OpusMode::Celt]);
        // The same with one byte less does not divide: refused.
        assert!(gate_packet(&cbr[..10], 1).is_err());
        // VBR, 2 frames: 3 bytes then the rest (2 bytes).
        assert!(gate_packet(&[CELT | 3, 0x80 | 2, 3, 1, 1, 1, 2, 2], 1).is_ok());
        // VBR whose last frame is 1 byte: refused.
        assert!(gate_packet(&[CELT | 3, 0x80 | 2, 3, 1, 1, 1, 2], 1).is_err());
        // Zero frames, and 7 x 20 ms (over 120 ms), are refused.
        assert!(gate_packet(&[CELT | 3, 0, 1, 1], 1).is_err());
        assert!(gate_packet(&[CELT | 3, 7, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1], 1).is_err());
        // Padding of 255 means 254 more and another length byte.
        let mut long = vec![CELT | 3, 0x40 | 1, 255, 0, 5, 5];
        long.extend(std::iter::repeat_n(0, 254));
        assert!(gate_packet(&long, 1).is_ok());
    }

    #[test]
    fn gate_walks_every_stream_of_a_multistream_packet() {
        // Stream 0 self-delimited (SILK, 3 bytes), stream 1 the rest (CELT, 2 bytes).
        let two = [SILK, 3, 1, 2, 3, CELT, 4, 5];
        assert_eq!(
            gate_packet(&two, 2).expect("two streams"),
            vec![OpusMode::Silk, OpusMode::Celt]
        );
        // A 1-byte frame in the first stream is caught although the packet is long.
        assert!(gate_packet(&[SILK, 1, 9, CELT, 4, 5, 6, 7], 2).is_err());
        // A last stream of only a TOC (0-byte frame) is caught.
        assert!(gate_packet(&[SILK, 3, 1, 2, 3, CELT], 2).is_err());
        // A self-delimited length past the end, and a missing stream, are refused.
        assert!(gate_packet(&[SILK, 40, 1, 2], 2).is_err());
        assert!(gate_packet(&[SILK, 2, 1, 2], 2).is_err());
    }

    /// A deterministic seeded walk over random packets: the gate never panics, and every packet it
    /// accepts has no frame of one byte or less in any stream.
    #[test]
    fn gate_survives_random_packets() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let mut accepted = 0;
        for _ in 0..20_000 {
            let len = (next() % 24) as usize;
            let bytes: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            let streams = 1 + (next() % 3) as u8;
            if let Ok(modes) = gate_packet(&bytes, streams) {
                accepted += 1;
                assert_eq!(modes.len(), usize::from(streams));
                assert!(bytes.len() >= 3 * usize::from(streams), "{bytes:?}");
            }
        }
        assert!(accepted > 0, "some random packets parse");
    }

    /// An encoder of the given application at 48 kHz mono, and 20 ms packets of a -12 dBFS tone.
    fn encode(application: i32, bitrate: i32, frames: usize, phase0: usize) -> Vec<Vec<u8>> {
        let mut err = 0i32;
        // SAFETY: `err` is a live local, read after the call.
        let enc = unsafe { unsafe_libopus::opus_encoder_create(48_000, 1, application, &mut err) };
        assert!(!enc.is_null() && err == 0, "encoder ({err})");
        // SAFETY: `enc` is the live encoder created above.
        let code = unsafe {
            unsafe_libopus::opus_encoder_ctl!(enc, unsafe_libopus::OPUS_SET_BITRATE_REQUEST, bitrate)
        };
        assert_eq!(code, 0);
        let mut packets = Vec::new();
        for f in 0..frames {
            let pcm: Vec<f32> = (0..960)
                .map(|i| {
                    let t = (phase0 + f * 960 + i) as f32 / 48_000.0;
                    0.25 * (std::f32::consts::TAU * 440.0 * t).sin()
                })
                .collect();
            let mut out = vec![0u8; 1500];
            // SAFETY: `pcm` holds the 960 samples passed, `out` has the 1500 bytes passed.
            let n =
                unsafe { unsafe_libopus::opus_encode_float(enc, pcm.as_ptr(), 960, out.as_mut_ptr(), 1500) };
            assert!(n > 0, "encode ({n})");
            out.truncate(n as usize);
            packets.push(out);
        }
        // SAFETY: created above, destroyed once.
        unsafe { unsafe_libopus::opus_encoder_destroy(enc) };
        packets
    }

    fn packet(data: &[u8], index: usize) -> Packet {
        Packet {
            track: 1,
            pts_ns: 1_000_000_000 + index as i64 * 20_000_000,
            duration_ns: None,
            keyframe: true,
            data: data.to_vec(),
        }
    }

    /// The hostile sequence the voice-real lane found (ADR-1911): speech in SILK, then a lost-frame
    /// packet of one and of two bytes, then CELT-only music, then SILK again. Through the gated
    /// decoder the -12 dBFS tone never comes near full scale; straight through libopus (no gate) the
    /// same packets do, which is the library defect the gate exists for.
    #[test]
    fn gated_decoder_never_blasts_on_concealment_triggers() {
        let silk = encode(unsafe_libopus::OPUS_APPLICATION_VOIP, 12_000, 25, 0);
        assert!(
            silk.iter().all(|p| OpusMode::of_toc(p[0]) == OpusMode::Silk),
            "VOIP at 12 kb/s is SILK"
        );
        let celt = encode(
            unsafe_libopus::OPUS_APPLICATION_RESTRICTED_LOWDELAY,
            64_000,
            10,
            25 * 960,
        );
        assert!(celt.iter().all(|p| OpusMode::of_toc(p[0]) == OpusMode::Celt));
        let more_silk = encode(unsafe_libopus::OPUS_APPLICATION_VOIP, 12_000, 25, 35 * 960);
        let tiny_1 = vec![silk[0][0]];
        let tiny_2 = vec![silk[0][0], 0x5a];
        let sequence: Vec<&Vec<u8>> = silk
            .iter()
            .chain([&tiny_1, &tiny_2])
            .chain(celt.iter())
            .chain(more_silk.iter())
            .collect();

        let h = OpusHead::parse(&head(1, 0, &[])).expect("head");
        let mut gated = OpusDecoder {
            gain: h.gain(),
            skip: 0,
            pcm: vec![0.0; MAX_FRAME],
            raw: Raw::open(&h).expect("libopus state"),
            modes: vec![None; usize::from(h.streams)],
            head: h.clone(),
            flushed: false,
        };
        let mut out = Vec::new();
        let mut refused = 0;
        for (i, p) in sequence.iter().enumerate() {
            let before = out.len();
            if gated.decode(&packet(p, i), &mut out).is_err() {
                refused += 1;
            }
            if std::ptr::eq(*p, &tiny_1) || std::ptr::eq(*p, &tiny_2) {
                // The two tiny packets never reach libopus: they decode as silence of their
                // duration (audit v034 cook M1, ADR-2385; integration ADR-2451).
                let silent = &out[before..];
                assert!(!silent.is_empty(), "packet {i}: its duration is kept");
                assert!(silent.iter().all(|v| *v == 0.0), "packet {i}: silence");
            }
        }
        assert_eq!(refused, 0, "no packet of the sequence is refused");
        let peak = out.iter().fold(0f32, |m, v| m.max(v.abs()));
        assert!(peak < 0.6, "gated peak {peak}");

        // The same packets straight into libopus: the concealment paths run.
        let raw = Raw::open(&h).expect("libopus state");
        let mut pcm = vec![0f32; MAX_FRAME];
        let mut raw_peak = 0f32;
        for p in &sequence {
            // SAFETY: `pcm` holds MAX_FRAME floats (mono), the frame size passed; `p` is valid for its length.
            let n = unsafe {
                unsafe_libopus::opus_multistream_decode_float(
                    raw.0,
                    p.as_ptr(),
                    p.len() as i32,
                    pcm.as_mut_ptr(),
                    MAX_FRAME as i32,
                    0,
                )
            };
            if n > 0 {
                raw_peak = pcm[..n as usize].iter().fold(raw_peak, |m, v| m.max(v.abs()));
            }
        }
        assert!(
            raw_peak > 0.9,
            "ungated peak {raw_peak}: if libopus no longer blasts here, the gate may be relaxed (ADR-1911)"
        );
    }

    /// The SILK-to-CELT switch alone, without the encoder's redundancy frame (two encoders, so the
    /// CELT one never saw the SILK frames): straight through libopus the transition crossfade starts
    /// at the concealment's full scale, the signature the decoder looks for; through the decoder the
    /// packet is decoded cold and the tone stays near its level.
    #[test]
    fn a_silk_to_celt_switch_without_redundancy_is_decoded_cold() {
        let silk = encode(unsafe_libopus::OPUS_APPLICATION_VOIP, 12_000, 25, 0);
        let celt = encode(
            unsafe_libopus::OPUS_APPLICATION_RESTRICTED_LOWDELAY,
            64_000,
            10,
            25 * 960,
        );
        let h = OpusHead::parse(&head(1, 0, &[])).expect("head");

        let raw = Raw::open(&h).expect("libopus state");
        let mut pcm = vec![0f32; MAX_FRAME];
        let mut first_window_peak = 0f32;
        for (i, p) in silk.iter().chain(celt.iter()).enumerate() {
            // SAFETY: `pcm` holds MAX_FRAME floats (mono), the frame size passed; `p` is valid for its length.
            let n = unsafe {
                unsafe_libopus::opus_multistream_decode_float(
                    raw.0,
                    p.as_ptr(),
                    p.len() as i32,
                    pcm.as_mut_ptr(),
                    MAX_FRAME as i32,
                    0,
                )
            };
            assert!(n > 0, "packet {i}: {n}");
            if i == silk.len() {
                first_window_peak = pcm[..TRANSITION_SAMPLES].iter().fold(0f32, |m, v| m.max(*v));
            }
        }
        assert!(
            first_window_peak >= CONCEALMENT_BLAST,
            "libopus's transition crossfade peaked at {first_window_peak}: the blast signature changed (ADR-2306)"
        );

        let mut decoder = OpusDecoder::from_head(h).expect("decoder");
        decoder.flushed = false;
        let mut out = Vec::new();
        for (i, p) in silk.iter().chain(celt.iter()).enumerate() {
            decoder
                .decode(&packet(p, i), &mut out)
                .expect("every packet decodes");
        }
        let peak = out.iter().fold(0f32, |m, v| m.max(v.abs()));
        assert!(peak < 0.6, "decoded peak {peak}");
    }
}
