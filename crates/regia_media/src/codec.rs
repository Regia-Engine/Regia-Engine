// SPDX-License-Identifier: MPL-2.0
//! Codecs: the codec rule (specs/50 § 1), the decoder traits, decoded pictures, and the two reference
//! codecs that ship in every build: raw planar video (`V_UNCOMPRESSED`, FourCC `I420`) and 32-bit float
//! PCM audio (`A_PCM/FLOAT/IEEE`). They are royalty-free, trivially exact, and let every player path
//! (state machine, lockstep, seek, loop, sync) run in tests without a native library. Compressed codecs
//! (AV1, Opus, Vorbis) plug in behind the same traits under the `video` feature, one sub-feature per codec
//! (`av1`, `opus`, `vorbis`; ADR-1593, ADR-1670, ADR-1869). [`codecs`] lists what this build decodes.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::container::{Packet, TrackInfo};
use crate::error::MediaError;
use crate::isolate::DecodeCaps;

/// What the codec rule says about a codec id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodecVerdict {
    /// Royalty-free with a decoder in this build.
    Supported,
    /// Royalty-free and allowed, but no decoder is built in (`codec_unsupported` with a build hint).
    NoDecoder,
    /// Patent-pooled: refused at open and import (`codec_patented`).
    Patented,
    /// Unknown or not in the allowed set (`codec_unsupported`).
    Unknown,
}

/// Matroska codec ids of patent-pooled or proprietary codecs (prefix match).
const PATENTED_PREFIXES: &[&str] = &[
    "V_MPEG4/ISO/",
    "V_MPEGH/ISO/",
    "V_MPEG1",
    "V_MPEG2",
    "V_MS/VFW/",
    "V_REAL/",
    "V_QUICKTIME",
    "A_AAC",
    "A_MPEG/",
    "A_AC3",
    "A_EAC3",
    "A_DTS",
    "A_TRUEHD",
    "A_REAL/",
    "A_MS/ACM",
    "A_QUICKTIME",
];

/// Raw video codec id.
pub const RAW_VIDEO: &str = "V_UNCOMPRESSED";
/// Float PCM codec id.
pub const PCM_FLOAT: &str = "A_PCM/FLOAT/IEEE";

/// AV1 codec id.
pub const AV1: &str = "V_AV1";
/// VP9 codec id (allowed, no decoder yet: specs/50 § 11).
pub const VP9: &str = "V_VP9";
/// Opus codec id.
pub const OPUS: &str = "A_OPUS";
/// Vorbis codec id.
pub const VORBIS: &str = "A_VORBIS";

/// Whether this build has a decoder for an allowed compressed codec id.
fn built_in(codec_id: &str) -> bool {
    (codec_id == AV1 && cfg!(feature = "av1"))
        || (codec_id == OPUS && cfg!(feature = "opus"))
        || (codec_id == VORBIS && cfg!(feature = "vorbis"))
}

/// Applies the codec rule to a Matroska codec id.
pub fn classify(codec_id: &str) -> CodecVerdict {
    match codec_id {
        RAW_VIDEO | PCM_FLOAT => CodecVerdict::Supported,
        AV1 | VP9 | OPUS | VORBIS if built_in(codec_id) => CodecVerdict::Supported,
        AV1 | VP9 | OPUS | VORBIS => CodecVerdict::NoDecoder,
        id if PATENTED_PREFIXES.iter().any(|p| id.starts_with(p)) => CodecVerdict::Patented,
        _ => CodecVerdict::Unknown,
    }
}

/// One row of [`codecs`]: an allowed codec and the decoder this build has for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodecSupport {
    /// Matroska codec id.
    pub codec_id: String,
    /// Short codec name (`av1`, `opus`, ...).
    pub codec: String,
    /// `video` or `audio`.
    pub kind: String,
    /// The decoder's name, or `None` when this build has none.
    pub decoder: Option<String>,
    /// The cargo feature that adds the decoder (`None` for the always-built reference codecs).
    pub feature: Option<String>,
    /// Whether the decoder may serve deterministic paths (`Lockstep`, render-out, goldens): every
    /// built-in decoder is a software reference (ADR-1593 § 2).
    pub deterministic: bool,
}

/// Whether the AV1 decoder of this build runs hand-written assembly: always on aarch64, on x86 with the
/// opt-in `av1-nasm` feature (ADR-1869 § 2). It changes speed only, never the decoded picture.
pub fn av1_assembly() -> bool {
    cfg!(feature = "av1") && (cfg!(target_arch = "aarch64") || cfg!(feature = "av1-nasm"))
}

/// The allowed codecs and the decoders built into this binary (the `video.codecs` agent method).
pub fn codecs() -> Vec<CodecSupport> {
    let row = |codec_id: &str, codec: &str, kind: &str, decoder: Option<&str>, feature: Option<&str>| {
        CodecSupport {
            codec_id: codec_id.to_string(),
            codec: codec.to_string(),
            kind: kind.to_string(),
            decoder: decoder.map(str::to_string),
            feature: feature.map(str::to_string),
            deterministic: decoder.is_some(),
        }
    };
    let when = |on: bool, name: &'static str| on.then_some(name);
    vec![
        row(
            AV1,
            "av1",
            "video",
            when(cfg!(feature = "av1"), "rav1d"),
            Some("av1"),
        ),
        row(VP9, "vp9", "video", None, None),
        row(RAW_VIDEO, "raw_i420", "video", Some("raw"), None),
        row(
            OPUS,
            "opus",
            "audio",
            when(cfg!(feature = "opus"), "libopus"),
            Some("opus"),
        ),
        row(
            VORBIS,
            "vorbis",
            "audio",
            when(cfg!(feature = "vorbis"), "lewton"),
            Some("vorbis"),
        ),
        row(PCM_FLOAT, "pcm_f32", "audio", Some("pcm"), None),
    ]
}

/// The error the codec rule gives for a codec id, or `None` when it decodes.
pub fn verdict_error(codec_id: &str) -> Option<MediaError> {
    match classify(codec_id) {
        CodecVerdict::Supported => None,
        CodecVerdict::Patented => Some(MediaError::CodecPatented(codec_id.to_string())),
        CodecVerdict::NoDecoder => Some(MediaError::CodecUnsupported {
            codec: codec_id.to_string(),
            hint: match codec_id {
                VP9 => "; no VP9 decoder exists yet (specs/50 § 11)".to_string(),
                _ => "; this build has no decoder for it: enable the regia feature `media-codecs` \
                      (regia_media `video`, ADR-1869)"
                    .to_string(),
            },
        }),
        CodecVerdict::Unknown => Some(MediaError::CodecUnsupported {
            codec: codec_id.to_string(),
            hint: String::new(),
        }),
    }
}

/// Chroma layout of a planar picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ChromaFormat {
    /// Half width, half height chroma.
    #[default]
    Yuv420,
    /// Half width, full height chroma.
    Yuv422,
    /// Full resolution chroma.
    Yuv444,
}

impl ChromaFormat {
    /// Horizontal and vertical chroma subsampling shifts.
    pub fn shifts(self) -> (u32, u32) {
        match self {
            Self::Yuv420 => (1, 1),
            Self::Yuv422 => (1, 0),
            Self::Yuv444 => (0, 0),
        }
    }

    /// Size of a chroma plane for a picture of `w` x `h` (rounded up).
    pub fn chroma_size(self, w: u32, h: u32) -> (u32, u32) {
        let (sx, sy) = self.shifts();
        (w.div_ceil(1 << sx), h.div_ceil(1 << sy))
    }
}

/// Color description from the clip (ITU-T H.273 code points).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColorInfo {
    /// Matrix coefficients (1 BT.709, 5/6 BT.601, 9 BT.2020 NCL, 2 unspecified).
    pub matrix: u8,
    /// Transfer characteristics (1 BT.709, 16 PQ, 18 HLG, 2 unspecified).
    pub transfer: u8,
    /// Color primaries (1 BT.709, 9 BT.2020, 2 unspecified).
    pub primaries: u8,
    /// Full range (0..=255) instead of limited (16..=235 luma).
    pub full_range: bool,
}

impl Default for ColorInfo {
    /// BT.709, limited range: the default when the clip says nothing (specs/50 § 2.7).
    fn default() -> Self {
        Self {
            matrix: 1,
            transfer: 1,
            primaries: 1,
            full_range: false,
        }
    }
}

/// A decoded planar YUV picture in presentation order. Samples above 8 bits are stored as
/// little-endian `u16` pairs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedPicture {
    /// Presentation time in nanoseconds.
    pub pts_ns: i64,
    /// Luma width.
    pub width: u32,
    /// Luma height.
    pub height: u32,
    /// 8 or 10 (12 accepted).
    pub bit_depth: u8,
    /// Chroma layout.
    pub chroma: ChromaFormat,
    /// Color description.
    pub color: ColorInfo,
    /// Y, U, V planes, tightly packed rows.
    ///
    /// Each plane is a shared allocation. A GPU submit clones the handle; it does not copy
    /// the samples. Cloning a picture clones the handles.
    pub planes: [Arc<[u8]>; 3],
}

impl DecodedPicture {
    /// Bytes per sample (1 for 8-bit, 2 above).
    pub fn sample_bytes(&self) -> usize {
        if self.bit_depth > 8 { 2 } else { 1 }
    }

    /// One sample of plane `p` at `(x, y)` in that plane's own resolution, clamped to the edges.
    pub fn sample(&self, p: usize, x: u32, y: u32) -> u16 {
        let (w, h) = if p == 0 {
            (self.width, self.height)
        } else {
            self.chroma.chroma_size(self.width, self.height)
        };
        let (x, y) = (x.min(w.saturating_sub(1)), y.min(h.saturating_sub(1)));
        let i = (y as usize * w as usize + x as usize) * self.sample_bytes();
        let plane = &self.planes[p];
        if self.sample_bytes() == 2 {
            match (plane.get(i), plane.get(i + 1)) {
                (Some(lo), Some(hi)) => u16::from_le_bytes([*lo, *hi]),
                _ => 0,
            }
        } else {
            plane.get(i).map_or(0, |v| u16::from(*v))
        }
    }

    /// Bytes held by the planes.
    pub fn byte_size(&self) -> usize {
        self.planes.iter().map(|plane| plane.len()).sum()
    }
}

/// A video decoder. Implementations may be native (ADR-1593); callers reach them only through the
/// isolation guard (`isolate::GuardedVideo`), which enforces the caps before every call.
pub trait VideoDecoder: Send {
    /// Decodes one packet, returning zero or more pictures in presentation order.
    fn decode(&mut self, packet: &Packet) -> Result<Vec<DecodedPicture>, MediaError>;
    /// Drops internal state (after a seek).
    fn flush(&mut self);
    /// A short name for logs and the benchmark ("raw", "rav1d", ...).
    fn name(&self) -> &'static str;
    /// Returns the pictures still held after the last packet (end of stream). Decoders that output
    /// every picture from `decode` keep the default.
    fn drain(&mut self) -> Result<Vec<DecodedPicture>, MediaError> {
        Ok(Vec::new())
    }
}

/// An audio decoder producing interleaved `f32` samples at the track's rate and channel count.
pub trait AudioDecoder: Send {
    /// Decodes one packet, appending interleaved samples to `out`.
    fn decode(&mut self, packet: &Packet, out: &mut Vec<f32>) -> Result<(), MediaError>;
    /// Drops internal state (after a seek).
    fn flush(&mut self);
    /// Channels and sample rate of the output when they differ from the track header (Opus always
    /// decodes at 48 kHz; Vorbis takes both from its identification header). `None`: the track's.
    fn output_format(&self) -> Option<(u8, u32)> {
        None
    }
}

/// Decoder for `V_UNCOMPRESSED` with FourCC `I420`: 8-bit 4:2:0 planar, Y then U then V.
#[derive(Debug, Clone)]
pub struct RawVideoDecoder {
    width: u32,
    height: u32,
    color: ColorInfo,
}

impl RawVideoDecoder {
    /// A decoder for the given video track.
    pub fn new(track: &TrackInfo) -> Result<Self, MediaError> {
        let video = track
            .video
            .as_ref()
            .ok_or_else(|| MediaError::invalid("raw video track without a Video element"))?;
        if let Some(fourcc) = &video.fourcc
            && fourcc != "I420"
        {
            return Err(MediaError::CodecUnsupported {
                codec: format!("{RAW_VIDEO} {fourcc}"),
                hint: "; raw video must be I420".to_string(),
            });
        }
        Ok(Self {
            width: video.width,
            height: video.height,
            color: video.color,
        })
    }

    /// Bytes of one I420 frame.
    pub fn frame_bytes(width: u32, height: u32) -> usize {
        let (cw, ch) = ChromaFormat::Yuv420.chroma_size(width, height);
        width as usize * height as usize + 2 * cw as usize * ch as usize
    }
}

impl VideoDecoder for RawVideoDecoder {
    fn decode(&mut self, packet: &Packet) -> Result<Vec<DecodedPicture>, MediaError> {
        let luma = self.width as usize * self.height as usize;
        if packet.data.len() != Self::frame_bytes(self.width, self.height) {
            return Err(MediaError::DecodeFailed(format!(
                "raw frame of {} bytes, expected {}",
                packet.data.len(),
                Self::frame_bytes(self.width, self.height)
            )));
        }
        let chroma = (packet.data.len() - luma) / 2;
        let (y, uv) = packet.data.split_at(luma);
        let (u, v) = uv.split_at(chroma);
        Ok(vec![DecodedPicture {
            pts_ns: packet.pts_ns,
            width: self.width,
            height: self.height,
            bit_depth: 8,
            chroma: ChromaFormat::Yuv420,
            color: self.color,
            planes: [Arc::from(y), Arc::from(u), Arc::from(v)],
        }])
    }

    fn flush(&mut self) {}

    fn name(&self) -> &'static str {
        "raw"
    }
}

/// Decoder for `A_PCM/FLOAT/IEEE` (32-bit little-endian interleaved floats).
#[derive(Debug, Clone, Default)]
pub struct PcmFloatDecoder;

impl AudioDecoder for PcmFloatDecoder {
    fn decode(&mut self, packet: &Packet, out: &mut Vec<f32>) -> Result<(), MediaError> {
        let (frames, rest) = packet.data.as_chunks::<4>();
        if !rest.is_empty() {
            return Err(MediaError::DecodeFailed(
                "float PCM packet not a multiple of 4 bytes".into(),
            ));
        }
        out.extend(frames.iter().map(|b| {
            let v = f32::from_le_bytes(*b);
            if v.is_finite() { v.clamp(-1.0, 1.0) } else { 0.0 }
        }));
        Ok(())
    }

    fn flush(&mut self) {}
}

/// Builds the built-in video decoder for a track, or the codec rule's error.
pub fn video_decoder_for(track: &TrackInfo) -> Result<Box<dyn VideoDecoder>, MediaError> {
    video_decoder_with(track, &DecodeCaps::default())
}

/// Builds the built-in video decoder for a track with the session's caps (decoders that can bound
/// their own allocations take them too), or the codec rule's error.
pub fn video_decoder_with(track: &TrackInfo, caps: &DecodeCaps) -> Result<Box<dyn VideoDecoder>, MediaError> {
    if let Some(err) = verdict_error(&track.codec_id) {
        return Err(err);
    }
    #[cfg(not(feature = "av1"))]
    let _ = caps;
    match track.codec_id.as_str() {
        RAW_VIDEO => Ok(Box::new(RawVideoDecoder::new(track)?)),
        #[cfg(feature = "av1")]
        AV1 => Ok(Box::new(crate::av1::Av1Decoder::new(track, caps)?)),
        other => Err(MediaError::CodecUnsupported {
            codec: other.to_string(),
            hint: "; not a video codec".to_string(),
        }),
    }
}

/// Builds the built-in audio decoder for a track, or the codec rule's error.
pub fn audio_decoder_for(track: &TrackInfo) -> Result<Box<dyn AudioDecoder>, MediaError> {
    if let Some(err) = verdict_error(&track.codec_id) {
        return Err(err);
    }
    match track.codec_id.as_str() {
        PCM_FLOAT => Ok(Box::new(PcmFloatDecoder)),
        #[cfg(feature = "opus")]
        OPUS => Ok(Box::new(crate::opus::OpusDecoder::new(track)?)),
        #[cfg(feature = "vorbis")]
        VORBIS => Ok(Box::new(crate::vorbis::VorbisDecoder::new(track)?)),
        other => Err(MediaError::CodecUnsupported {
            codec: other.to_string(),
            hint: "; not an audio codec".to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_rule_table() {
        assert_eq!(classify("V_MPEG4/ISO/AVC"), CodecVerdict::Patented);
        assert_eq!(classify("V_MPEGH/ISO/HEVC"), CodecVerdict::Patented);
        assert_eq!(classify("A_AAC/MPEG4/LC"), CodecVerdict::Patented);
        assert_eq!(classify("A_MPEG/L3"), CodecVerdict::Patented);
        let av1 = if cfg!(feature = "av1") {
            CodecVerdict::Supported
        } else {
            CodecVerdict::NoDecoder
        };
        let opus = if cfg!(feature = "opus") {
            CodecVerdict::Supported
        } else {
            CodecVerdict::NoDecoder
        };
        assert_eq!(classify("V_AV1"), av1);
        assert_eq!(classify("A_OPUS"), opus);
        assert_eq!(classify("V_VP9"), CodecVerdict::NoDecoder);
        assert_eq!(classify("V_VP8"), CodecVerdict::Unknown);
        assert_eq!(classify(RAW_VIDEO), CodecVerdict::Supported);
        assert_eq!(
            verdict_error("V_MPEG4/ISO/AVC").map(|e| e.code()),
            Some("codec_patented")
        );
        assert_eq!(
            verdict_error("V_VP9").map(|e| e.code()),
            Some("codec_unsupported")
        );
        assert_eq!(verdict_error("V_AV1").is_none(), cfg!(feature = "av1"));
    }

    #[test]
    fn codec_table_matches_the_rule() {
        let table = codecs();
        assert_eq!(table.len(), 6);
        for row in &table {
            let decodes = classify(&row.codec_id) == CodecVerdict::Supported;
            assert_eq!(row.decoder.is_some(), decodes, "{}", row.codec_id);
            assert_eq!(row.deterministic, decodes);
            assert!(row.kind == "video" || row.kind == "audio");
        }
    }

    #[test]
    fn chroma_sizes_round_up() {
        assert_eq!(ChromaFormat::Yuv420.chroma_size(5, 3), (3, 2));
        assert_eq!(ChromaFormat::Yuv422.chroma_size(5, 3), (3, 3));
        assert_eq!(RawVideoDecoder::frame_bytes(4, 2), 8 + 2 * 2);
    }
}
