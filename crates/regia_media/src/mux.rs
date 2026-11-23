// SPDX-License-Identifier: MPL-2.0
//! A small WebM / Matroska writer for synthetic fixtures (specs/50 § 8): deterministic pattern clips in
//! raw I420 video and float PCM audio, used by the tests, the benchmark and the agent examples. It is a
//! muxer, so it stays Rust (ADR-1593 § 1). Not a general-purpose encoder: no compression.
//!
//! [`WebmWriter`] is the streaming writer of the encoders (`crate::encode`, ADR-3369): AV1 and Opus
//! tracks with their codec fields, keyframe clusters, cues, exact durations and the Opus discard
//! padding, written cluster by cluster to any `Write + Seek`.

use crate::codec::{PCM_FLOAT, RAW_VIDEO, RawVideoDecoder};
use crate::container::{Marker, TrackKind, id};
use crate::ebml::{uint_bytes, write_element};
use std::io::{Seek, Write};

/// A track to write.
#[derive(Debug, Clone, PartialEq)]
pub struct MuxTrack {
    /// Track number (1-based).
    pub number: u64,
    /// Kind.
    pub kind: TrackKind,
    /// Matroska codec id.
    pub codec_id: String,
    /// Nominal frame duration.
    pub default_duration_ns: Option<u64>,
    /// Language tag.
    pub language: String,
    /// Default flag.
    pub default: bool,
    /// Video width, height and raw FourCC.
    pub video: Option<(u32, u32, Option<String>)>,
    /// Audio sample rate and channels.
    pub audio: Option<(f64, u8)>,
}

/// A packet to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MuxPacket {
    /// Track number.
    pub track: u64,
    /// Presentation time in nanoseconds.
    pub pts_ns: i64,
    /// Keyframe flag.
    pub keyframe: bool,
    /// Payload.
    pub data: Vec<u8>,
}

/// Writer options.
#[derive(Debug, Clone, PartialEq)]
pub struct MuxOptions {
    /// `webm` or `matroska`.
    pub doc_type: String,
    /// Nanoseconds per timestamp unit (1 ms is the Matroska default).
    pub timestamp_scale: u64,
    /// Write a Cues element (one cue per cluster that opens on a video keyframe).
    pub cues: bool,
    /// Declared duration.
    pub duration_ns: Option<u64>,
    /// Chapter markers.
    pub markers: Vec<Marker>,
}

impl Default for MuxOptions {
    fn default() -> Self {
        Self {
            doc_type: "webm".to_string(),
            timestamp_scale: 1_000_000,
            cues: true,
            duration_ns: None,
            markers: Vec::new(),
        }
    }
}

fn uint_el(out: &mut Vec<u8>, eid: u32, v: u64) {
    write_element(out, eid, &uint_bytes(v));
}

fn track_entry(t: &MuxTrack) -> Vec<u8> {
    let mut e = Vec::new();
    uint_el(&mut e, id::TRACK_NUMBER, t.number);
    let kind = match t.kind {
        TrackKind::Video => 1,
        TrackKind::Audio => 2,
        TrackKind::Text => 17,
        TrackKind::Other => 3,
    };
    uint_el(&mut e, id::TRACK_TYPE, kind);
    uint_el(&mut e, id::FLAG_DEFAULT, u64::from(t.default));
    write_element(&mut e, id::LANGUAGE, t.language.as_bytes());
    write_element(&mut e, id::CODEC_ID, t.codec_id.as_bytes());
    if let Some(d) = t.default_duration_ns {
        uint_el(&mut e, id::DEFAULT_DURATION, d);
    }
    if let Some((w, h, fourcc)) = &t.video {
        let mut v = Vec::new();
        uint_el(&mut v, id::PIXEL_WIDTH, u64::from(*w));
        uint_el(&mut v, id::PIXEL_HEIGHT, u64::from(*h));
        if let Some(f) = fourcc {
            write_element(&mut v, id::UNCOMPRESSED_FOURCC, f.as_bytes());
        }
        write_element(&mut e, id::VIDEO, &v);
    }
    if let Some((rate, channels)) = t.audio {
        let mut a = Vec::new();
        write_element(&mut a, id::SAMPLING_FREQUENCY, &rate.to_bits().to_be_bytes());
        uint_el(&mut a, id::CHANNELS, u64::from(channels));
        uint_el(&mut a, id::BIT_DEPTH, 32);
        write_element(&mut e, id::AUDIO, &a);
    }
    e
}

fn simple_block(p: &MuxPacket, rel: i16) -> Vec<u8> {
    let mut b = Vec::with_capacity(p.data.len() + 8);
    // Track number as a 1-byte vint (fixtures use tracks below 127).
    b.push(0x80 | (p.track.min(126) as u8));
    b.extend_from_slice(&rel.to_be_bytes());
    b.push(if p.keyframe { 0x80 } else { 0x00 });
    b.extend_from_slice(&p.data);
    b
}

/// A text cue to write (a `BlockGroup` with a `BlockDuration`, ADR-2410).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MuxCue {
    /// Track number of the text track.
    pub track: u64,
    /// Start in nanoseconds.
    pub pts_ns: i64,
    /// Duration in nanoseconds (`None` writes a `SimpleBlock` without one).
    pub duration_ns: Option<u64>,
    /// The cue text.
    pub text: String,
}

/// Writes a complete file. `packets` must be in presentation order per track; a new cluster opens at
/// every video keyframe and whenever a relative timestamp would leave the `i16` range.
pub fn mux(opts: &MuxOptions, tracks: &[MuxTrack], packets: &[MuxPacket]) -> Vec<u8> {
    let items: Vec<(MuxPacket, Option<u64>)> = packets.iter().map(|p| (p.clone(), None)).collect();
    mux_items(opts, tracks, &items)
}

/// [`mux`] plus text cues, merged into the packet stream by start time (a cue goes before a packet
/// of the same time).
pub fn mux_with_cues(
    opts: &MuxOptions,
    tracks: &[MuxTrack],
    packets: &[MuxPacket],
    cues: &[MuxCue],
) -> Vec<u8> {
    let mut items: Vec<(MuxPacket, Option<u64>)> = Vec::with_capacity(packets.len() + cues.len());
    let mut cues: Vec<&MuxCue> = cues.iter().collect();
    cues.sort_by_key(|c| c.pts_ns);
    let mut next = cues.into_iter().peekable();
    for p in packets {
        while let Some(c) = next.next_if(|c| c.pts_ns <= p.pts_ns) {
            items.push(cue_item(c));
        }
        items.push((p.clone(), None));
    }
    for c in next {
        items.push(cue_item(c));
    }
    mux_items(opts, tracks, &items)
}

fn cue_item(c: &MuxCue) -> (MuxPacket, Option<u64>) {
    (
        MuxPacket {
            track: c.track,
            pts_ns: c.pts_ns,
            keyframe: true,
            data: c.text.as_bytes().to_vec(),
        },
        c.duration_ns,
    )
}

fn mux_items(opts: &MuxOptions, tracks: &[MuxTrack], packets: &[(MuxPacket, Option<u64>)]) -> Vec<u8> {
    let scale = opts.timestamp_scale.max(1) as i64;
    let video = tracks
        .iter()
        .find(|t| t.kind == TrackKind::Video)
        .map(|t| t.number);
    let mut body = Vec::new();

    let mut info = Vec::new();
    uint_el(&mut info, id::TIMESTAMP_SCALE, opts.timestamp_scale.max(1));
    if let Some(d) = opts.duration_ns {
        let units = d as f64 / opts.timestamp_scale.max(1) as f64;
        write_element(&mut info, id::DURATION, &units.to_bits().to_be_bytes());
    }
    write_element(&mut body, id::INFO, &info);

    let mut tr = Vec::new();
    for t in tracks {
        write_element(&mut tr, id::TRACK_ENTRY, &track_entry(t));
    }
    write_element(&mut body, id::TRACKS, &tr);

    if !opts.markers.is_empty() {
        let mut edition = Vec::new();
        for m in &opts.markers {
            let mut display = Vec::new();
            write_element(&mut display, id::CHAP_STRING, m.name.as_bytes());
            let mut atom = Vec::new();
            uint_el(&mut atom, id::CHAPTER_TIME_START, m.time_ns);
            write_element(&mut atom, id::CHAPTER_DISPLAY, &display);
            write_element(&mut edition, id::CHAPTER_ATOM, &atom);
        }
        let mut chapters = Vec::new();
        write_element(&mut chapters, id::EDITION_ENTRY, &edition);
        write_element(&mut body, id::CHAPTERS, &chapters);
    }

    let mut cues: Vec<(i64, u64)> = Vec::new();
    let mut cluster: Vec<u8> = Vec::new();
    let mut cluster_ts: Option<i64> = None;
    let flush = |body: &mut Vec<u8>, cluster: &mut Vec<u8>| {
        if !cluster.is_empty() {
            write_element(body, id::CLUSTER, cluster);
            cluster.clear();
        }
    };
    for (p, duration) in packets {
        let units = p.pts_ns.div_euclid(scale);
        let video_key = Some(p.track) == video && p.keyframe;
        let out_of_range = cluster_ts.is_some_and(|c| units - c > i64::from(i16::MAX) || units < c);
        if cluster_ts.is_none() || video_key || out_of_range {
            flush(&mut body, &mut cluster);
            cluster_ts = Some(units);
            uint_el(&mut cluster, id::TIMESTAMP, units.max(0) as u64);
            if video_key {
                cues.push((units, body.len() as u64));
            }
        }
        let rel = (units - cluster_ts.unwrap_or(units)) as i16;
        match duration {
            Some(d) => {
                let mut group = Vec::new();
                let mut block = simple_block(p, rel);
                // A Block has no keyframe flag; the group's lack of ReferenceBlock marks it.
                block[3] = 0;
                write_element(&mut group, id::BLOCK, &block);
                uint_el(&mut group, id::BLOCK_DURATION, d / opts.timestamp_scale.max(1));
                write_element(&mut cluster, id::BLOCK_GROUP, &group);
            }
            None => write_element(&mut cluster, id::SIMPLE_BLOCK, &simple_block(p, rel)),
        }
    }
    flush(&mut body, &mut cluster);

    if opts.cues && !cues.is_empty() {
        let mut c = Vec::new();
        for (time, pos) in &cues {
            let mut ctp = Vec::new();
            uint_el(&mut ctp, id::CUE_TRACK, video.unwrap_or(1));
            uint_el(&mut ctp, id::CUE_CLUSTER_POSITION, *pos);
            let mut point = Vec::new();
            uint_el(&mut point, id::CUE_TIME, (*time).max(0) as u64);
            write_element(&mut point, id::CUE_TRACK_POSITIONS, &ctp);
            write_element(&mut c, id::CUE_POINT, &point);
        }
        write_element(&mut body, id::CUES, &c);
    }

    let mut header = Vec::new();
    uint_el(&mut header, 0x4286, 1); // EBMLVersion
    uint_el(&mut header, 0x42F7, 1); // EBMLReadVersion
    uint_el(&mut header, 0x42F2, 4); // EBMLMaxIDLength
    uint_el(&mut header, 0x42F3, 8); // EBMLMaxSizeLength
    write_element(&mut header, id::DOC_TYPE, opts.doc_type.as_bytes());
    uint_el(&mut header, 0x4287, 4); // DocTypeVersion
    uint_el(&mut header, 0x4285, 2); // DocTypeReadVersion
    let mut out = Vec::new();
    write_element(&mut out, id::EBML, &header);
    write_element(&mut out, id::SEGMENT, &body);
    out
}

/// A tone for the pattern clip's audio track.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ToneAudio {
    /// Sample rate.
    pub rate: u32,
    /// Channels.
    pub channels: u8,
    /// Tone frequency.
    pub freq_hz: f32,
    /// Peak amplitude.
    pub amplitude: f32,
}

/// A deterministic pattern clip: raw I420 frames whose content is a function of the frame index, and
/// an optional float PCM sine tone.
#[derive(Debug, Clone, PartialEq)]
pub struct PatternClip {
    /// Width (even).
    pub width: u32,
    /// Height (even).
    pub height: u32,
    /// Frame count.
    pub frames: u32,
    /// Frame duration in nanoseconds.
    pub frame_ns: u64,
    /// Keyframe interval (every frame of raw video is decodable alone; this only sets cues).
    pub keyframe_every: u32,
    /// Optional audio.
    pub audio: Option<ToneAudio>,
    /// Write cues.
    pub cues: bool,
    /// Video codec id (raw unless a test wants a refused codec).
    pub video_codec: String,
}

impl PatternClip {
    /// A silent clip of `frames` frames at `fps`.
    pub fn new(width: u32, height: u32, frames: u32, fps: u32) -> Self {
        Self {
            width,
            height,
            frames,
            frame_ns: 1_000_000_000 / u64::from(fps.max(1)),
            keyframe_every: 5,
            audio: None,
            cues: true,
            video_codec: RAW_VIDEO.to_string(),
        }
    }

    /// Presentation time of frame `k`, quantized to the 1 ms timestamp scale exactly as written.
    pub fn frame_pts_ns(&self, k: u32) -> i64 {
        let ns = u64::from(k) * self.frame_ns;
        ((ns + 500_000) / 1_000_000 * 1_000_000) as i64
    }

    /// Declared duration (`frames * frame_ns`).
    pub fn duration_ns(&self) -> u64 {
        u64::from(self.frames) * self.frame_ns
    }

    /// Writes the clip with a UTF-8 text track (number 3, codec `S_TEXT/UTF8`, language `eng`)
    /// carrying `cues` as (start ns, duration ns, text).
    pub fn to_webm_with_cues(&self, cues: &[(i64, Option<u64>, &str)]) -> Vec<u8> {
        let (mut tracks, packets, opts) = self.parts();
        tracks.push(MuxTrack {
            number: 3,
            kind: TrackKind::Text,
            codec_id: "S_TEXT/UTF8".to_string(),
            default_duration_ns: None,
            language: "eng".to_string(),
            default: true,
            video: None,
            audio: None,
        });
        let cues: Vec<MuxCue> = cues
            .iter()
            .map(|(pts_ns, duration_ns, text)| MuxCue {
                track: 3,
                pts_ns: *pts_ns,
                duration_ns: *duration_ns,
                text: (*text).to_string(),
            })
            .collect();
        mux_with_cues(&opts, &tracks, &packets, &cues)
    }

    /// Writes the clip.
    pub fn to_webm(&self) -> Vec<u8> {
        let (tracks, packets, opts) = self.parts();
        mux(&opts, &tracks, &packets)
    }

    fn parts(&self) -> (Vec<MuxTrack>, Vec<MuxPacket>, MuxOptions) {
        let mut tracks = vec![MuxTrack {
            number: 1,
            kind: TrackKind::Video,
            codec_id: self.video_codec.clone(),
            default_duration_ns: Some(self.frame_ns),
            language: "und".to_string(),
            default: true,
            video: Some((self.width, self.height, Some("I420".to_string()))),
            audio: None,
        }];
        if let Some(a) = self.audio {
            tracks.push(MuxTrack {
                number: 2,
                kind: TrackKind::Audio,
                codec_id: PCM_FLOAT.to_string(),
                default_duration_ns: None,
                language: "eng".to_string(),
                default: true,
                video: None,
                audio: Some((f64::from(a.rate), a.channels)),
            });
        }
        let mut packets = Vec::new();
        for k in 0..self.frames {
            let pts = self.frame_pts_ns(k);
            packets.push(MuxPacket {
                track: 1,
                pts_ns: pts,
                keyframe: k.is_multiple_of(self.keyframe_every.max(1)),
                data: pattern_frame(self.width, self.height, k),
            });
            if let Some(a) = self.audio {
                packets.push(MuxPacket {
                    track: 2,
                    pts_ns: pts,
                    keyframe: true,
                    data: tone_packet(a, self.frame_ns, k),
                });
            }
        }
        let opts = MuxOptions {
            cues: self.cues,
            duration_ns: Some(self.duration_ns()),
            ..MuxOptions::default()
        };
        (tracks, packets, opts)
    }
}

/// The I420 bytes of pattern frame `k`. Luma bytes 0 and 1 carry `k` (low, high) so a test can read
/// back which frame is shown; the rest is a moving gradient.
pub fn pattern_frame(width: u32, height: u32, k: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(RawVideoDecoder::frame_bytes(width, height));
    for y in 0..height {
        for x in 0..width {
            out.push((x.wrapping_add(y.wrapping_mul(3)).wrapping_add(k.wrapping_mul(7)) & 0xFF) as u8);
        }
    }
    if out.len() >= 2 {
        out[0] = (k & 0xFF) as u8;
        out[1] = ((k >> 8) & 0xFF) as u8;
    }
    let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
    for _ in 0..ch {
        for x in 0..cw {
            out.push((k.wrapping_mul(13).wrapping_add(x) & 0xFF) as u8);
        }
    }
    for y in 0..ch {
        for _ in 0..cw {
            out.push((k.wrapping_mul(29).wrapping_add(y) & 0xFF) as u8);
        }
    }
    out
}

/// The frame index a pattern picture carries in its first two luma bytes.
pub fn pattern_index(luma: &[u8]) -> u32 {
    match luma {
        [lo, hi, ..] => u32::from(*lo) | (u32::from(*hi) << 8),
        _ => 0,
    }
}

/// Float PCM samples of the tone for video frame `k`'s span (integer sample bounds, so packets tile).
pub fn tone_packet(a: ToneAudio, frame_ns: u64, k: u32) -> Vec<u8> {
    let start = u64::from(k) * frame_ns * u64::from(a.rate) / 1_000_000_000;
    let end = u64::from(k + 1) * frame_ns * u64::from(a.rate) / 1_000_000_000;
    let mut out = Vec::with_capacity(((end - start) * u64::from(a.channels) * 4) as usize);
    for s in start..end {
        let phase = (s as f64) * f64::from(a.freq_hz) / f64::from(a.rate);
        let v = (std::f64::consts::TAU * phase).sin() as f32 * a.amplitude;
        for _ in 0..a.channels {
            out.extend_from_slice(&v.to_le_bytes());
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------------
// The streaming WebM writer of the encoders (spec 50 § 14, ADR-3369).
// ---------------------------------------------------------------------------------------------------

/// Element IDs the streaming writer needs beyond the demuxer's table (Matroska specification).
mod wid {
    pub(super) const SEEK_HEAD: u32 = 0x114D_9B74;
    pub(super) const SEEK: u32 = 0x4DBB;
    pub(super) const SEEK_ID: u32 = 0x53AB;
    pub(super) const SEEK_POSITION: u32 = 0x53AC;
    pub(super) const VOID: u32 = 0xEC;
    pub(super) const MUXING_APP: u32 = 0x4D80;
    pub(super) const WRITING_APP: u32 = 0x5741;
    pub(super) const TRACK_UID: u32 = 0x73C5;
    pub(super) const FLAG_LACING: u32 = 0x9C;
    pub(super) const CODEC_DELAY: u32 = 0x56AA;
    pub(super) const SEEK_PRE_ROLL: u32 = 0x56BB;
    pub(super) const DISCARD_PADDING: u32 = 0x75A2;
    pub(super) const CHROMA_SITING_HORZ: u32 = 0x55B7;
    pub(super) const CHROMA_SITING_VERT: u32 = 0x55B8;
}

/// The color description of a video track (Matroska `Colour`, code points of ITU-T H.273).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebmColour {
    /// `MatrixCoefficients` (1: BT.709).
    pub matrix: u8,
    /// `Range` (1: broadcast / limited, 2: full).
    pub range: u8,
    /// `TransferCharacteristics` (13: sRGB).
    pub transfer: u8,
    /// `Primaries` (1: BT.709).
    pub primaries: u8,
    /// `ChromaSitingHorz` and `ChromaSitingVert` (1: top-left / co-sited, 2: half).
    pub chroma_siting: (u8, u8),
}

impl WebmColour {
    /// BT.709 limited range with the sRGB transfer and center-sited chroma: what
    /// [`crate::encode::rgba_to_i420`] produces.
    pub const SRGB_BT709_LIMITED: Self = Self {
        matrix: 1,
        range: 1,
        transfer: 13,
        primaries: 1,
        chroma_siting: (2, 2),
    };
}

/// A track of a streamed WebM file.
#[derive(Debug, Clone, PartialEq)]
pub struct WebmTrack {
    /// Track number, 1..=126.
    pub number: u64,
    /// Kind (video or audio).
    pub kind: TrackKind,
    /// Matroska codec id (`V_AV1`, `A_OPUS`).
    pub codec_id: String,
    /// CodecPrivate (`av1C`, `OpusHead`); empty writes none.
    pub codec_private: Vec<u8>,
    /// `CodecDelay` in nanoseconds (the Opus pre-skip); 0 writes none.
    pub codec_delay_ns: u64,
    /// `SeekPreRoll` in nanoseconds (80 ms for Opus); 0 writes none.
    pub seek_pre_roll_ns: u64,
    /// `DefaultDuration` in nanoseconds.
    pub default_duration_ns: Option<u64>,
    /// Language tag.
    pub language: String,
    /// Picture width and height, and the color description.
    pub video: Option<(u32, u32, Option<WebmColour>)>,
    /// Sample rate and channels.
    pub audio: Option<(f64, u8)>,
}

/// A block to write: one AV1 temporal unit or one Opus packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebmBlock {
    /// Track number.
    pub track: u64,
    /// Presentation time in nanoseconds (0 or later; non-decreasing across all blocks).
    pub pts_ns: i64,
    /// Keyframe (sync point).
    pub keyframe: bool,
    /// `DiscardPadding` in nanoseconds (the samples to drop at the end of the last Opus packet); 0
    /// writes a `SimpleBlock`, anything else a `BlockGroup`.
    pub discard_padding_ns: i64,
    /// Payload.
    pub data: Vec<u8>,
}

/// What a streamed file holds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WebmStats {
    /// Blocks written.
    pub blocks: u64,
    /// Clusters written.
    pub clusters: u64,
    /// Cue points written.
    pub cues: u64,
    /// Bytes of the whole file.
    pub bytes: u64,
}

/// Longest cluster of a file without video, in timestamp units (5 s at the 1 ms scale).
const AUDIO_CLUSTER_UNITS: i64 = 5_000;
/// A cluster is closed once its blocks pass this many bytes (bounds the writer's memory).
const MAX_CLUSTER_BYTES: usize = 32 * 1024 * 1024;
/// The timestamp scale of streamed files: 1 ms (the WebM convention).
pub const WEBM_TIMESTAMP_SCALE: u64 = 1_000_000;

fn invalid_input(message: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, message.into())
}

/// An element with a fixed 8-byte unsigned body, so it can be patched in place.
fn fixed_uint_el(out: &mut Vec<u8>, eid: u32, v: u64) {
    write_element(out, eid, &v.to_be_bytes());
}

/// One `Seek` entry with a fixed-size position (42 bytes).
fn seek_entry(target: u32, position: u64) -> Vec<u8> {
    let mut seek = Vec::new();
    write_element(&mut seek, wid::SEEK_ID, &target.to_be_bytes());
    fixed_uint_el(&mut seek, wid::SEEK_POSITION, position);
    let mut out = Vec::new();
    write_element(&mut out, wid::SEEK, &seek);
    out
}

/// A WebM writer that streams clusters to `W` as they fill (ADR-3369).
///
/// The file is a regular WebM: EBML header, a Segment with a `SeekHead` (Info, Tracks, Cues), Info
/// with `MuxingApp`, `WritingApp` and the duration, Tracks with every codec field the AV1 and Opus
/// mappings ask for (`CodecPrivate`, `CodecDelay`, `SeekPreRoll`, `Colour`), clusters, and Cues at the
/// end. A new cluster opens at every video keyframe (each such cluster gets a cue point), when a
/// relative timestamp would leave the `i16` range, past 32 MiB, and every 5 s in a file without
/// video. Timestamps are in milliseconds ([`WEBM_TIMESTAMP_SCALE`]).
///
/// The Segment size, the duration and the Cues position are written as placeholders and patched by
/// [`Self::finish`]; until then the file holds every closed cluster and reads as a live (unknown-size)
/// segment.
pub struct WebmWriter<W: Write + Seek> {
    out: W,
    pos: u64,
    segment_size_at: u64,
    segment_data: u64,
    duration_at: u64,
    cues_seek_at: u64,
    video: Option<u64>,
    cluster: Vec<u8>,
    cluster_ts: Option<i64>,
    last_units: i64,
    cues: Vec<(i64, u64, u64)>,
    stats: WebmStats,
}

impl<W: Write + Seek> WebmWriter<W> {
    /// Writes the header, the `SeekHead`, Info and Tracks. `app` names the writer (`MuxingApp` and
    /// `WritingApp`).
    ///
    /// # Errors
    /// An invalid track list (none, a number outside 1..=126, a duplicate) or a write error.
    pub fn new(mut out: W, app: &str, tracks: &[WebmTrack]) -> std::io::Result<Self> {
        if tracks.is_empty() {
            return Err(invalid_input("a WebM file needs at least one track"));
        }
        for (i, t) in tracks.iter().enumerate() {
            if !(1..=126).contains(&t.number) || tracks[..i].iter().any(|o| o.number == t.number) {
                return Err(invalid_input(format!(
                    "bad or duplicate track number {}",
                    t.number
                )));
            }
        }
        let start = out.stream_position()?;
        let mut header = Vec::new();
        uint_el(&mut header, 0x4286, 1); // EBMLVersion
        uint_el(&mut header, 0x42F7, 1); // EBMLReadVersion
        uint_el(&mut header, 0x42F2, 4); // EBMLMaxIDLength
        uint_el(&mut header, 0x42F3, 8); // EBMLMaxSizeLength
        write_element(&mut header, id::DOC_TYPE, b"webm");
        uint_el(&mut header, 0x4287, 4); // DocTypeVersion
        uint_el(&mut header, 0x4285, 2); // DocTypeReadVersion
        let mut head = Vec::new();
        write_element(&mut head, id::EBML, &header);
        head.extend_from_slice(&id::SEGMENT.to_be_bytes());
        let segment_size_at = start + head.len() as u64;
        // Unknown size until `finish` patches it.
        head.extend_from_slice(&[0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
        let segment_data = start + head.len() as u64;

        let mut info = Vec::new();
        uint_el(&mut info, id::TIMESTAMP_SCALE, WEBM_TIMESTAMP_SCALE);
        write_element(&mut info, wid::MUXING_APP, app.as_bytes());
        write_element(&mut info, wid::WRITING_APP, app.as_bytes());
        let duration_in_info = info.len() + 2 + 8; // ID 0x4489 (2 bytes) + 8-byte size
        write_element(&mut info, id::DURATION, &0f64.to_bits().to_be_bytes());
        let mut info_el = Vec::new();
        write_element(&mut info_el, id::INFO, &info);
        let info_header = info_el.len() - info.len();

        let mut tr = Vec::new();
        for (i, t) in tracks.iter().enumerate() {
            write_element(&mut tr, id::TRACK_ENTRY, &webm_track_entry(t, i as u64));
        }
        let mut tracks_el = Vec::new();
        write_element(&mut tracks_el, id::TRACKS, &tr);

        // The SeekHead has a fixed size: three 42-byte entries.
        let seek_len = {
            let mut probe = Vec::new();
            write_element(&mut probe, wid::SEEK_HEAD, &[0u8; 3 * 42]);
            probe.len() as u64
        };
        let info_pos = seek_len;
        let tracks_pos = info_pos + info_el.len() as u64;
        let mut seeks = Vec::new();
        seeks.extend(seek_entry(id::INFO, info_pos));
        seeks.extend(seek_entry(id::TRACKS, tracks_pos));
        let cues_entry_at = seeks.len();
        seeks.extend(seek_entry(id::CUES, 0));
        let mut seek_head = Vec::new();
        write_element(&mut seek_head, wid::SEEK_HEAD, &seeks);
        debug_assert_eq!(seek_head.len() as u64, seek_len);
        let seek_body = seek_head.len() - seeks.len();

        let duration_at = segment_data + info_pos + (info_header + duration_in_info) as u64;
        let cues_seek_at = segment_data + (seek_body + cues_entry_at) as u64;
        head.extend_from_slice(&seek_head);
        head.extend_from_slice(&info_el);
        head.extend_from_slice(&tracks_el);
        out.write_all(&head)?;
        let video = tracks
            .iter()
            .find(|t| t.kind == TrackKind::Video)
            .map(|t| t.number);
        Ok(Self {
            out,
            pos: start + head.len() as u64,
            segment_size_at,
            segment_data,
            duration_at,
            cues_seek_at,
            video,
            cluster: Vec::new(),
            cluster_ts: None,
            last_units: 0,
            cues: Vec::new(),
            stats: WebmStats::default(),
        })
    }

    /// Adds a block. Blocks must come in presentation order across all tracks.
    ///
    /// # Errors
    /// A negative or decreasing timestamp, or a write error while closing the previous cluster.
    pub fn write(&mut self, block: &WebmBlock) -> std::io::Result<()> {
        if block.pts_ns < 0 {
            return Err(invalid_input("negative block timestamp"));
        }
        let units = block.pts_ns / WEBM_TIMESTAMP_SCALE as i64;
        if units < self.last_units {
            return Err(invalid_input(format!(
                "block at {} ms after one at {} ms: blocks must be in time order",
                units, self.last_units
            )));
        }
        self.last_units = units;
        let video_key = Some(block.track) == self.video && block.keyframe;
        let open = self.cluster_ts.is_some_and(|ts| {
            let full = (video_key && !self.cluster.is_empty())
                || units - ts > i64::from(i16::MAX)
                || self.cluster.len() > MAX_CLUSTER_BYTES
                || (self.video.is_none() && units - ts >= AUDIO_CLUSTER_UNITS);
            !full
        });
        if !open {
            self.close_cluster()?;
            self.cluster_ts = Some(units);
            uint_el(&mut self.cluster, id::TIMESTAMP, units as u64);
            let cue_track = match self.video {
                Some(v) if video_key => Some(v),
                None => Some(block.track),
                Some(_) => None,
            };
            if let Some(track) = cue_track {
                self.cues.push((units, self.pos - self.segment_data, track));
            }
        }
        let rel = (units - self.cluster_ts.unwrap_or(units)) as i16;
        let mut b = Vec::with_capacity(block.data.len() + 4);
        b.push(0x80 | (block.track as u8));
        b.extend_from_slice(&rel.to_be_bytes());
        if block.discard_padding_ns == 0 {
            b.push(if block.keyframe { 0x80 } else { 0x00 });
            b.extend_from_slice(&block.data);
            write_element(&mut self.cluster, id::SIMPLE_BLOCK, &b);
        } else {
            b.push(0x00);
            b.extend_from_slice(&block.data);
            let mut group = Vec::new();
            write_element(&mut group, id::BLOCK, &b);
            write_element(
                &mut group,
                wid::DISCARD_PADDING,
                &block.discard_padding_ns.to_be_bytes(),
            );
            if !block.keyframe {
                // A ReferenceBlock marks a group as not a keyframe (relative time of the reference).
                write_element(&mut group, id::REFERENCE_BLOCK, &[0xFF]);
            }
            write_element(&mut self.cluster, id::BLOCK_GROUP, &group);
        }
        self.stats.blocks += 1;
        Ok(())
    }

    fn close_cluster(&mut self) -> std::io::Result<()> {
        if self.cluster.is_empty() {
            return Ok(());
        }
        let mut el = Vec::with_capacity(self.cluster.len() + 12);
        write_element(&mut el, id::CLUSTER, &self.cluster);
        self.out.write_all(&el)?;
        self.pos += el.len() as u64;
        self.cluster.clear();
        self.cluster_ts = None;
        self.stats.clusters += 1;
        Ok(())
    }

    /// Statistics so far.
    pub fn stats(&self) -> WebmStats {
        self.stats
    }

    /// Closes the last cluster, writes the Cues and patches the duration, the Segment size and the
    /// Cues position. Returns the writer (positioned at the end) and what was written.
    ///
    /// # Errors
    /// A write or seek error.
    pub fn finish(mut self, duration_ns: u64) -> std::io::Result<(W, WebmStats)> {
        self.close_cluster()?;
        let cues_pos = self.pos - self.segment_data;
        if !self.cues.is_empty() {
            let mut c = Vec::new();
            for (time, pos, track) in &self.cues {
                let mut ctp = Vec::new();
                uint_el(&mut ctp, id::CUE_TRACK, *track);
                uint_el(&mut ctp, id::CUE_CLUSTER_POSITION, *pos);
                let mut point = Vec::new();
                uint_el(&mut point, id::CUE_TIME, *time as u64);
                write_element(&mut point, id::CUE_TRACK_POSITIONS, &ctp);
                write_element(&mut c, id::CUE_POINT, &point);
            }
            let mut el = Vec::new();
            write_element(&mut el, id::CUES, &c);
            self.out.write_all(&el)?;
            self.pos += el.len() as u64;
            self.stats.cues = self.cues.len() as u64;
        }
        let end = self.pos;
        let duration_units = duration_ns as f64 / WEBM_TIMESTAMP_SCALE as f64;
        self.out.seek(std::io::SeekFrom::Start(self.duration_at))?;
        self.out.write_all(&duration_units.to_bits().to_be_bytes())?;
        self.out.seek(std::io::SeekFrom::Start(self.segment_size_at))?;
        let mut size = vec![0x01];
        size.extend_from_slice(&(end - self.segment_data).to_be_bytes()[1..]);
        self.out.write_all(&size)?;
        self.out.seek(std::io::SeekFrom::Start(self.cues_seek_at))?;
        if self.cues.is_empty() {
            // No Cues: the entry becomes a Void of the same 42 bytes.
            let mut void = Vec::new();
            write_element(&mut void, wid::VOID, &[0u8; 42 - 9]);
            self.out.write_all(&void)?;
        } else {
            self.out.write_all(&seek_entry(id::CUES, cues_pos))?;
        }
        self.out.seek(std::io::SeekFrom::Start(end))?;
        self.out.flush()?;
        self.stats.bytes = end;
        Ok((self.out, self.stats))
    }
}

fn webm_track_entry(t: &WebmTrack, index: u64) -> Vec<u8> {
    let mut e = Vec::new();
    uint_el(&mut e, id::TRACK_NUMBER, t.number);
    // A stable UID per track: the number plus a fixed tag (no clock, no randomness).
    uint_el(
        &mut e,
        wid::TRACK_UID,
        0x5245_4749_0000_0000 | (index << 8) | t.number,
    );
    let kind = match t.kind {
        TrackKind::Video => 1,
        TrackKind::Audio => 2,
        TrackKind::Text => 17,
        TrackKind::Other => 3,
    };
    uint_el(&mut e, id::TRACK_TYPE, kind);
    uint_el(&mut e, wid::FLAG_LACING, 0);
    uint_el(&mut e, id::FLAG_DEFAULT, 1);
    write_element(&mut e, id::LANGUAGE, t.language.as_bytes());
    write_element(&mut e, id::CODEC_ID, t.codec_id.as_bytes());
    if !t.codec_private.is_empty() {
        write_element(&mut e, id::CODEC_PRIVATE, &t.codec_private);
    }
    if t.codec_delay_ns > 0 {
        uint_el(&mut e, wid::CODEC_DELAY, t.codec_delay_ns);
    }
    if t.seek_pre_roll_ns > 0 {
        uint_el(&mut e, wid::SEEK_PRE_ROLL, t.seek_pre_roll_ns);
    }
    if let Some(d) = t.default_duration_ns {
        uint_el(&mut e, id::DEFAULT_DURATION, d);
    }
    if let Some((w, h, colour)) = &t.video {
        let mut v = Vec::new();
        uint_el(&mut v, id::PIXEL_WIDTH, u64::from(*w));
        uint_el(&mut v, id::PIXEL_HEIGHT, u64::from(*h));
        if let Some(c) = colour {
            let mut col = Vec::new();
            uint_el(&mut col, id::MATRIX, u64::from(c.matrix));
            uint_el(&mut col, id::BITS_PER_CHANNEL, 8);
            uint_el(&mut col, wid::CHROMA_SITING_HORZ, u64::from(c.chroma_siting.0));
            uint_el(&mut col, wid::CHROMA_SITING_VERT, u64::from(c.chroma_siting.1));
            uint_el(&mut col, id::RANGE, u64::from(c.range));
            uint_el(&mut col, id::TRANSFER, u64::from(c.transfer));
            uint_el(&mut col, id::PRIMARIES, u64::from(c.primaries));
            write_element(&mut v, id::COLOUR, &col);
        }
        write_element(&mut e, id::VIDEO, &v);
    }
    if let Some((rate, channels)) = t.audio {
        let mut a = Vec::new();
        write_element(&mut a, id::SAMPLING_FREQUENCY, &rate.to_bits().to_be_bytes());
        uint_el(&mut a, id::CHANNELS, u64::from(channels));
        write_element(&mut e, id::AUDIO, &a);
    }
    e
}

#[cfg(test)]
mod webm_writer_tests {
    use super::*;
    use crate::container::{Demuxer, MkvDemuxer};
    use std::io::Cursor;

    fn tracks() -> Vec<WebmTrack> {
        vec![
            WebmTrack {
                number: 1,
                kind: TrackKind::Video,
                codec_id: RAW_VIDEO.to_string(),
                codec_private: Vec::new(),
                codec_delay_ns: 0,
                seek_pre_roll_ns: 0,
                default_duration_ns: Some(41_666_667),
                language: "und".into(),
                video: Some((16, 16, Some(WebmColour::SRGB_BT709_LIMITED))),
                audio: None,
            },
            WebmTrack {
                number: 2,
                kind: TrackKind::Audio,
                codec_id: PCM_FLOAT.to_string(),
                codec_private: b"private".to_vec(),
                codec_delay_ns: 6_500_000,
                seek_pre_roll_ns: 80_000_000,
                default_duration_ns: None,
                language: "eng".into(),
                video: None,
                audio: Some((48_000.0, 2)),
            },
        ]
    }

    fn write_file(blocks: &[WebmBlock], duration_ns: u64) -> (Vec<u8>, WebmStats) {
        let mut w = WebmWriter::new(Cursor::new(Vec::new()), "regia test", &tracks()).expect("header");
        for b in blocks {
            w.write(b).expect("block");
        }
        let (out, stats) = w.finish(duration_ns).expect("finish");
        (out.into_inner(), stats)
    }

    fn block(track: u64, pts_ns: i64, keyframe: bool, byte: u8) -> WebmBlock {
        WebmBlock {
            track,
            pts_ns,
            keyframe,
            discard_padding_ns: 0,
            data: vec![byte; 8],
        }
    }

    #[test]
    fn a_streamed_file_reads_back_with_tracks_cues_duration_and_every_block() {
        let mut blocks = Vec::new();
        for k in 0..48i64 {
            blocks.push(block(1, k * 41_666_667, k % 12 == 0, k as u8));
            blocks.push(block(2, k * 41_666_667, true, 200));
        }
        blocks.last_mut().expect("last").discard_padding_ns = 2_000_000;
        let (bytes, stats) = write_file(&blocks, 2_000_000_000);
        assert_eq!((stats.blocks, stats.clusters, stats.cues), (96, 4, 4));
        assert_eq!(stats.bytes, bytes.len() as u64);
        let mut d = MkvDemuxer::open(Cursor::new(bytes)).expect("demux");
        let info = d.probe().expect("probe");
        assert_eq!(info.duration_ns, Some(2_000_000_000));
        assert_eq!(info.cue_count, 4);
        assert_eq!(info.tracks.len(), 2);
        assert_eq!(info.tracks[1].codec_private, b"private");
        assert_eq!(info.tracks[0].default_duration_ns, Some(41_666_667));
        let v = info.tracks[0].video.as_ref().expect("video");
        assert_eq!((v.width, v.height), (16, 16));
        let mut seen = Vec::new();
        while let Some(p) = d.next_packet().expect("packet") {
            seen.push((p.track, p.pts_ns, p.keyframe, p.data[0]));
        }
        assert_eq!(seen.len(), 96);
        for (i, (track, pts, key, byte)) in seen.iter().enumerate() {
            let k = (i / 2) as i64;
            assert_eq!(*pts, k * 41_666_667 / 1_000_000 * 1_000_000, "block {i}");
            if *track == 1 {
                assert_eq!((*key, *byte), (k % 12 == 0, k as u8));
            } else {
                assert!(*key);
            }
        }
        // Seeking lands on a keyframe cluster.
        let landed = d.seek(1_100_000_000).expect("seek");
        assert_eq!(landed, 1_000_000_000);
    }

    #[test]
    fn out_of_order_and_negative_blocks_are_refused() {
        let mut w = WebmWriter::new(Cursor::new(Vec::new()), "t", &tracks()).expect("header");
        w.write(&block(1, 10_000_000, true, 0)).expect("first");
        assert!(w.write(&block(2, 5_000_000, true, 0)).is_err());
        assert!(w.write(&block(2, -1, true, 0)).is_err());
        assert!(WebmWriter::new(Cursor::new(Vec::new()), "t", &[]).is_err());
        let mut dup = tracks();
        dup[1].number = 1;
        assert!(WebmWriter::new(Cursor::new(Vec::new()), "t", &dup).is_err());
    }

    #[test]
    fn a_file_without_blocks_still_parses_and_long_gaps_split_clusters() {
        let (bytes, stats) = write_file(&[], 0);
        assert_eq!((stats.blocks, stats.cues), (0, 0));
        assert!(MkvDemuxer::open(Cursor::new(bytes)).is_ok());
        // 40 s between two blocks of one keyframe run: the i16 range forces a second cluster.
        let (bytes, stats) = write_file(
            &[block(1, 0, true, 1), block(1, 40_000_000_000, false, 2)],
            41_000_000_000,
        );
        assert_eq!((stats.clusters, stats.cues), (2, 1));
        let mut d = MkvDemuxer::open(Cursor::new(bytes)).expect("demux");
        let mut pts = Vec::new();
        while let Some(p) = d.next_packet().expect("packet") {
            pts.push(p.pts_ns);
        }
        assert_eq!(pts, vec![0, 40_000_000_000]);
    }

    #[test]
    fn the_unpatched_file_reads_as_a_live_segment_up_to_the_last_closed_cluster() {
        let mut w = WebmWriter::new(Cursor::new(Vec::new()), "t", &tracks()).expect("header");
        for k in 0..30i64 {
            w.write(&block(1, k * 40_000_000, k % 10 == 0, k as u8))
                .expect("block");
        }
        // Two clusters are closed (frames 0..20); frames 20..30 are still in memory.
        let partial = w.out.get_ref().clone();
        let mut d = MkvDemuxer::open(Cursor::new(partial)).expect("live file opens");
        let mut n = 0;
        while d.next_packet().expect("packet").is_some() {
            n += 1;
        }
        assert_eq!(n, 20);
    }
}
