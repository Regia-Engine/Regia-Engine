// SPDX-License-Identifier: MPL-2.0
//! WebM / Matroska demuxer (specs/50 § 4): the element subset of the EBML header, Segment Info,
//! Tracks, Cues, Chapters, Clusters with SimpleBlock and BlockGroup (all three lacing kinds).
//! Original code from the public Matroska and EBML specifications (`docs/provenance.md`).
//!
//! The file is untrusted. Lengths are checked against the remaining file and the 64 MiB cap
//! ([`crate::ebml::MAX_ALLOC`]) before allocation; a malformed level-1 element is skipped by its size when
//! the size is plausible, otherwise the demuxer resynchronizes at the next Cluster ID. A truncated final
//! cluster ends the stream early without an error. A read error while bytes remain is `io_failed`. An
//! element id that is not a legal EBML vint is skipped by resynchronizing at the next Cluster. Encryption
//! is refused (`container_invalid`).

use std::collections::VecDeque;
use std::io::{Read, Seek};

use serde::{Deserialize, Serialize};

use crate::codec::{self, ChromaFormat, CodecVerdict, ColorInfo};
use crate::ebml::{self, Cursor, Source};
use crate::error::MediaError;

/// Element IDs used by the demuxer and the fixture muxer (Matroska specification).
pub(crate) mod id {
    pub(crate) const EBML: u32 = 0x1A45_DFA3;
    pub(crate) const DOC_TYPE: u32 = 0x4282;
    pub(crate) const SEGMENT: u32 = 0x1853_8067;
    pub(crate) const INFO: u32 = 0x1549_A966;
    pub(crate) const TIMESTAMP_SCALE: u32 = 0x2A_D7B1;
    pub(crate) const DURATION: u32 = 0x4489;
    pub(crate) const TRACKS: u32 = 0x1654_AE6B;
    pub(crate) const TRACK_ENTRY: u32 = 0xAE;
    pub(crate) const TRACK_NUMBER: u32 = 0xD7;
    pub(crate) const TRACK_TYPE: u32 = 0x83;
    pub(crate) const FLAG_DEFAULT: u32 = 0x88;
    pub(crate) const LANGUAGE: u32 = 0x22_B59C;
    pub(crate) const CODEC_ID: u32 = 0x86;
    pub(crate) const CODEC_PRIVATE: u32 = 0x63A2;
    pub(crate) const DEFAULT_DURATION: u32 = 0x23_E383;
    pub(crate) const CONTENT_ENCODINGS: u32 = 0x6D80;
    pub(crate) const VIDEO: u32 = 0xE0;
    pub(crate) const PIXEL_WIDTH: u32 = 0xB0;
    pub(crate) const PIXEL_HEIGHT: u32 = 0xBA;
    pub(crate) const DISPLAY_WIDTH: u32 = 0x54B0;
    pub(crate) const DISPLAY_HEIGHT: u32 = 0x54BA;
    pub(crate) const UNCOMPRESSED_FOURCC: u32 = 0x2E_B524;
    pub(crate) const COLOUR: u32 = 0x55B0;
    pub(crate) const MATRIX: u32 = 0x55B1;
    pub(crate) const BITS_PER_CHANNEL: u32 = 0x55B2;
    pub(crate) const CHROMA_SUB_HORZ: u32 = 0x55B3;
    pub(crate) const CHROMA_SUB_VERT: u32 = 0x55B4;
    pub(crate) const RANGE: u32 = 0x55B9;
    pub(crate) const TRANSFER: u32 = 0x55BA;
    pub(crate) const PRIMARIES: u32 = 0x55BB;
    pub(crate) const AUDIO: u32 = 0xE1;
    pub(crate) const SAMPLING_FREQUENCY: u32 = 0xB5;
    pub(crate) const CHANNELS: u32 = 0x9F;
    pub(crate) const BIT_DEPTH: u32 = 0x6264;
    pub(crate) const CLUSTER: u32 = 0x1F43_B675;
    pub(crate) const TIMESTAMP: u32 = 0xE7;
    pub(crate) const SIMPLE_BLOCK: u32 = 0xA3;
    pub(crate) const BLOCK_GROUP: u32 = 0xA0;
    pub(crate) const BLOCK: u32 = 0xA1;
    pub(crate) const BLOCK_DURATION: u32 = 0x9B;
    pub(crate) const REFERENCE_BLOCK: u32 = 0xFB;
    pub(crate) const CUES: u32 = 0x1C53_BB6B;
    pub(crate) const CUE_POINT: u32 = 0xBB;
    pub(crate) const CUE_TIME: u32 = 0xB3;
    pub(crate) const CUE_TRACK_POSITIONS: u32 = 0xB7;
    pub(crate) const CUE_TRACK: u32 = 0xF7;
    pub(crate) const CUE_CLUSTER_POSITION: u32 = 0xF1;
    pub(crate) const CHAPTERS: u32 = 0x1043_A770;
    pub(crate) const EDITION_ENTRY: u32 = 0x45B9;
    pub(crate) const CHAPTER_ATOM: u32 = 0xB6;
    pub(crate) const CHAPTER_TIME_START: u32 = 0x91;
    pub(crate) const CHAPTER_DISPLAY: u32 = 0x80;
    pub(crate) const CHAP_STRING: u32 = 0x85;
    pub(crate) const TAGS: u32 = 0x1254_C367;
}

/// Most tracks a file may declare.
const MAX_TRACKS: usize = 64;
/// Most channels an audio track may have, in the container and in a codec's own header.
pub const MAX_AUDIO_CHANNELS: u8 = 8;
/// Most cue points kept.
const MAX_CUES: usize = 1 << 20;
/// Most markers kept.
const MAX_MARKERS: usize = 4096;
/// Most frames one laced block may carry.
const MAX_LACED: usize = 256;
/// Shortest nonzero `Duration` a file may declare (1 ms): a shorter one is `container_invalid`
/// (ADR-7563, deep-zai-media finding 1).
pub const MIN_DECLARED_DURATION_NS: u64 = 1_000_000;

/// Container family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContainerKind {
    /// DocType `webm`.
    Webm,
    /// DocType `matroska`.
    Matroska,
}

/// Kind of a track.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackKind {
    /// Picture.
    Video,
    /// Sound.
    Audio,
    /// Subtitles, captions, metadata (text cues).
    Text,
    /// Anything else.
    Other,
}

/// Video track parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VideoParams {
    /// Coded width.
    pub width: u32,
    /// Coded height.
    pub height: u32,
    /// Display width (defaults to the coded width).
    pub display_width: u32,
    /// Display height (defaults to the coded height).
    pub display_height: u32,
    /// Bits per channel (8 when absent).
    pub bit_depth: u8,
    /// Chroma layout (4:2:0 when absent).
    pub chroma: ChromaFormat,
    /// Color description.
    pub color: ColorInfo,
    /// FourCC of raw video, if any.
    pub fourcc: Option<String>,
}

/// Audio track parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioParams {
    /// Sample rate in Hz.
    pub sample_rate: f64,
    /// Channel count.
    pub channels: u8,
    /// Bit depth (0 when absent).
    pub bit_depth: u8,
}

impl Default for AudioParams {
    /// The Matroska defaults of an Audio element's fields (one channel, 8000 Hz), which also stand
    /// for a track that has no Audio element at all.
    fn default() -> Self {
        Self {
            sample_rate: 8000.0,
            channels: 1,
            bit_depth: 0,
        }
    }
}

/// One track of the file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackInfo {
    /// Matroska track number (block addressing).
    pub number: u64,
    /// Index in the track list.
    pub index: u32,
    /// Kind.
    pub kind: TrackKind,
    /// Matroska codec id (`V_AV1`, `A_OPUS`, ...).
    pub codec_id: String,
    /// Codec setup bytes.
    #[serde(skip)]
    pub codec_private: Vec<u8>,
    /// Language tag (`eng` when absent: the Matroska default of the Language element).
    pub language: String,
    /// The default flag.
    pub default: bool,
    /// Nominal frame or packet duration in nanoseconds.
    pub default_duration_ns: Option<u64>,
    /// Nominal frame rate (from the default duration).
    pub frame_rate: Option<f64>,
    /// Video parameters.
    pub video: Option<VideoParams>,
    /// Audio parameters.
    pub audio: Option<AudioParams>,
    /// What the codec rule says.
    pub verdict: CodecVerdict,
}

/// ISO 639-1 with the 639-2/T and 639-2/B tags used in Matroska, plus BCP-47 primaries that are not
/// the 639-1 code. Norwegian bokmål (`nb`, `nob`) and nynorsk (`nn`, `nno`) each share `no`/`nor`
/// and do not match each other. Legacy `iw` is Hebrew and legacy `in` is Indonesian. A BCP-47
/// primary subtag matches any tag in its row. `und` matches nothing, including another `und`.
const ISO639: &[&[&str]] = &[
    &["en", "eng"],
    &["fr", "fra", "fre"],
    &["de", "deu", "ger"],
    &["es", "spa"],
    &["pt", "por"],
    &["it", "ita"],
    &["nl", "nld", "dut"],
    &["ru", "rus"],
    &["ja", "jpn"],
    &["zh", "zho", "chi"],
    &["ko", "kor"],
    &["ar", "ara"],
    &["hi", "hin"],
    &["sv", "swe"],
    &["no", "nb", "nor", "nob"],
    &["no", "nn", "nor", "nno"],
    &["da", "dan"],
    &["fi", "fin"],
    &["pl", "pol"],
    &["cs", "ces", "cze"],
    &["el", "ell", "gre"],
    &["hu", "hun"],
    &["ro", "ron", "rum"],
    &["uk", "ukr"],
    &["tr", "tur"],
    &["he", "heb", "iw"],
    &["th", "tha"],
    &["vi", "vie"],
    &["id", "ind", "in"],
    &["ms", "msa", "may"],
];

fn language_primary(tag: &str) -> String {
    let tag = tag.trim();
    let end = tag.find(['-', '_']).unwrap_or(tag.len());
    tag[..end].to_ascii_lowercase()
}

/// Whether `want` (a BCP-47 tag or an ISO 639 code) and a track's `language` are the same language.
/// The compare is the primary subtag, then the ISO 639 table above. `und` never matches.
pub fn languages_match(want: &str, track: &str) -> bool {
    let want = language_primary(want);
    let track = language_primary(track);
    if want.is_empty() || track.is_empty() || want == "und" || track == "und" {
        return false;
    }
    if want == track {
        return true;
    }
    ISO639
        .iter()
        .any(|row| row.contains(&want.as_str()) && row.contains(&track.as_str()))
}

/// A chapter start, exposed as a marker by time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marker {
    /// Chapter title.
    pub name: String,
    /// Start in nanoseconds.
    pub time_ns: u64,
}

/// What `probe` reports.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaInfo {
    /// Container family.
    pub container: ContainerKind,
    /// Duration in nanoseconds (from Segment Info), if declared.
    pub duration_ns: Option<u64>,
    /// Nanoseconds per timestamp unit.
    pub timestamp_scale: u64,
    /// Tracks in file order.
    pub tracks: Vec<TrackInfo>,
    /// Cue points found.
    pub cue_count: usize,
    /// Chapter markers.
    pub markers: Vec<Marker>,
}

impl MediaInfo {
    /// The first video track.
    pub fn video_track(&self) -> Option<&TrackInfo> {
        self.tracks.iter().find(|t| t.kind == TrackKind::Video)
    }

    /// The audio track chosen by default: the one flagged default, else the first whose language
    /// [`languages_match`]es `language`, else the first (specs/50 § 2.3).
    pub fn default_audio_track(&self, language: &str) -> Option<&TrackInfo> {
        let audio: Vec<&TrackInfo> = self
            .tracks
            .iter()
            .filter(|t| t.kind == TrackKind::Audio)
            .collect();
        audio
            .iter()
            .find(|t| t.default)
            .or_else(|| audio.iter().find(|t| languages_match(language, &t.language)))
            .or_else(|| audio.first())
            .copied()
    }

    /// The first refusal of the codec rule across video and audio tracks: a patented codec anywhere,
    /// or a first video track without a decoder.
    pub fn reject_reason(&self) -> Option<MediaError> {
        for t in &self.tracks {
            if matches!(t.kind, TrackKind::Video | TrackKind::Audio) && t.verdict == CodecVerdict::Patented {
                return Some(MediaError::CodecPatented(t.codec_id.clone()));
            }
        }
        match self.video_track() {
            None => Some(MediaError::invalid("no video track")),
            Some(v) => codec::verdict_error(&v.codec_id),
        }
    }
}

/// A demuxed packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    /// Matroska track number.
    pub track: u64,
    /// Presentation time in nanoseconds.
    pub pts_ns: i64,
    /// Block duration, if the block carried one.
    pub duration_ns: Option<u64>,
    /// Keyframe (sync point).
    pub keyframe: bool,
    /// Payload.
    pub data: Vec<u8>,
}

/// How far laced frame `index` of a block sits after its first frame. A `DefaultDuration` too large
/// to give a usable time counts as none.
fn lace_offset_ns(default_duration_ns: Option<u64>, index: usize) -> i64 {
    default_duration_ns
        .and_then(|d| d.checked_mul(index as u64))
        .and_then(|v| i64::try_from(v).ok())
        .unwrap_or(0)
}

/// A container reader (specs/50 § 3.2).
pub trait Demuxer: Send {
    /// File information (parsed at open).
    fn probe(&mut self) -> Result<MediaInfo, MediaError>;
    /// The next packet in file order, `None` at the end.
    fn next_packet(&mut self) -> Result<Option<Packet>, MediaError>;
    /// Moves to the cue point at or before `to_ns`; returns the cue time landed on.
    fn seek(&mut self, to_ns: u64) -> Result<u64, MediaError>;
}

#[derive(Debug, Clone, Copy)]
struct CuePoint {
    time_ns: u64,
    pos: u64,
}

#[derive(Debug, Clone, Copy)]
struct ClusterState {
    end: Option<u64>,
    timestamp: i64,
}

/// What `MkvDemuxer::read_header` decided.
enum HeaderRead {
    /// A parsed element header.
    Header(ebml::Header),
    /// A bad vint was skipped and the source sits on a later Cluster.
    Resynced,
    /// The source is exhausted, or a short tail that ends playback cleanly.
    End,
}

/// Demuxer counters (corruption handling).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DemuxStats {
    /// Packets returned.
    pub packets: u64,
    /// Times the demuxer had to scan for the next Cluster.
    pub resyncs: u64,
    /// Elements skipped as malformed.
    pub skipped: u64,
}

/// The Matroska / WebM demuxer.
pub struct MkvDemuxer<R> {
    src: Source<R>,
    info: MediaInfo,
    seg_data: u64,
    seg_end: u64,
    first_cluster: Option<u64>,
    cues: Vec<CuePoint>,
    cluster: Option<ClusterState>,
    pending: VecDeque<Packet>,
    video_track: Option<u64>,
    drop_until_key: bool,
    stats: DemuxStats,
}

impl<R> std::fmt::Debug for MkvDemuxer<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MkvDemuxer")
            .field("info", &self.info)
            .finish_non_exhaustive()
    }
}

fn sniff_foreign(head: &[u8]) -> Option<&'static str> {
    if head.len() >= 8 && &head[4..8] == b"ftyp" {
        return Some("MP4/MOV");
    }
    if head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"AVI " {
        return Some("AVI");
    }
    if head.first() == Some(&0x47) && head.get(188) == Some(&0x47) {
        return Some("MPEG transport stream");
    }
    if head.len() >= 3 && &head[..3] == b"FLV" {
        return Some("FLV");
    }
    if head.len() >= 4 && head[..4] == [0x30, 0x26, 0xB2, 0x75] {
        return Some("ASF/WMV");
    }
    None
}

/// Refuses known non-Matroska extensions (`container_unsupported`) before the file is opened.
pub fn check_extension(path: &str) -> Result<(), MediaError> {
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "mp4" | "m4v" | "mov" | "avi" | "ts" | "m2ts" | "flv" | "wmv" | "3gp" | "mpg" | "mpeg" => {
            Err(MediaError::ContainerUnsupported(format!(".{ext}")))
        }
        _ => Ok(()),
    }
}

impl<R: Read + Seek> MkvDemuxer<R> {
    /// Opens a file: parses the EBML header and every level-1 element up to the first Cluster, then
    /// looks for Cues after the clusters when none came before.
    pub fn open(reader: R) -> Result<Self, MediaError> {
        let mut src = Source::new(reader)?;
        let head_len = src.len().min(192);
        let head = src.read_vec(head_len)?;
        if let Some(kind) = sniff_foreign(&head) {
            return Err(MediaError::ContainerUnsupported(kind.to_string()));
        }
        src.seek_to(0)?;
        if head.len() < 4 || ebml::uint(&head[..4])? != u64::from(id::EBML) {
            return Err(MediaError::invalid("not an EBML file"));
        }
        let ebml_head = src.header()?;
        let size = ebml_head
            .size
            .ok_or_else(|| MediaError::invalid("EBML header of unknown size"))?;
        let body = src.read_vec(size)?;
        let mut doc_type = String::from("matroska");
        let mut c = Cursor::new(&body);
        while !c.is_empty() {
            let (eid, data) = c.element()?;
            if eid == id::DOC_TYPE {
                doc_type = ebml::string(data);
            }
        }
        let container = match doc_type.as_str() {
            "webm" => ContainerKind::Webm,
            "matroska" => ContainerKind::Matroska,
            other => return Err(MediaError::ContainerUnsupported(format!("DocType {other}"))),
        };
        let seg = src.header()?;
        if seg.id != id::SEGMENT {
            return Err(MediaError::invalid("no Segment after the EBML header"));
        }
        let seg_end = seg
            .size
            .map_or(src.len(), |s| seg.data_start.saturating_add(s).min(src.len()));
        let mut demux = Self {
            src,
            info: MediaInfo {
                container,
                duration_ns: None,
                timestamp_scale: 1_000_000,
                tracks: Vec::new(),
                cue_count: 0,
                markers: Vec::new(),
            },
            seg_data: seg.data_start,
            seg_end,
            first_cluster: None,
            cues: Vec::new(),
            cluster: None,
            pending: VecDeque::new(),
            video_track: None,
            drop_until_key: false,
            stats: DemuxStats::default(),
        };
        demux.read_metadata()?;
        if demux.info.tracks.is_empty() {
            return Err(MediaError::invalid("no Tracks element before the first Cluster"));
        }
        demux.video_track = demux.info.video_track().map(|t| t.number);
        demux.info.cue_count = demux.cues.len();
        demux.rewind()?;
        Ok(demux)
    }

    /// Counters.
    pub fn stats(&self) -> DemuxStats {
        self.stats
    }

    /// The parsed file information.
    pub fn info(&self) -> &MediaInfo {
        &self.info
    }

    fn rewind(&mut self) -> Result<(), MediaError> {
        self.pending.clear();
        self.cluster = None;
        self.src.seek_to(self.first_cluster.unwrap_or(self.seg_end))
    }

    /// Walks level-1 elements: metadata before the first Cluster, then skips clusters (by size) to
    /// find trailing Cues and Chapters.
    fn read_metadata(&mut self) -> Result<(), MediaError> {
        self.src.seek_to(self.seg_data)?;
        let mut walked_clusters = 0usize;
        while self.src.pos() < self.seg_end {
            let at = self.src.pos();
            let h = match self.src.header() {
                Ok(h) => h,
                // A read error is not "there is no Tracks element". A short or malformed tail still
                // stops the walk: clusters after it are read when playback starts.
                Err(MediaError::IoFailed(msg)) => return Err(MediaError::IoFailed(msg)),
                Err(_) => break,
            };
            if h.id == id::CLUSTER {
                if self.first_cluster.is_none() {
                    self.first_cluster = Some(at);
                }
                walked_clusters += 1;
                // An unknown-size cluster (live file): stop here; there are no trailing Cues we can find
                // without scanning every block.
                let Some(size) = h.size else { break };
                if walked_clusters > 1_000_000 {
                    break;
                }
                self.src.seek_to(h.data_start.saturating_add(size))?;
                continue;
            }
            let Some(size) = h.size.filter(|s| *s <= self.src.remaining()) else {
                // Implausible size outside clusters: stop the metadata walk; streaming resyncs later.
                self.stats.skipped += 1;
                break;
            };
            match h.id {
                id::INFO | id::TRACKS | id::CUES | id::CHAPTERS => {
                    let body = self.src.read_vec(size)?;
                    match h.id {
                        id::INFO => self.parse_info(&body)?,
                        id::TRACKS => self.parse_tracks(&body)?,
                        id::CUES => self.parse_cues(&body).unwrap_or_else(|_| self.stats.skipped += 1),
                        _ => self
                            .parse_chapters(&body)
                            .unwrap_or_else(|_| self.stats.skipped += 1),
                    }
                }
                _ => self.src.skip(size)?,
            }
        }
        Ok(())
    }

    fn scale(&self, units: i64) -> i64 {
        (i128::from(units) * i128::from(self.info.timestamp_scale)).clamp(i64::MIN.into(), i64::MAX.into())
            as i64
    }

    fn parse_info(&mut self, body: &[u8]) -> Result<(), MediaError> {
        let mut c = Cursor::new(body);
        let mut duration_units = None;
        while !c.is_empty() {
            let (eid, data) = c.element()?;
            match eid {
                id::TIMESTAMP_SCALE => {
                    let s = ebml::uint(data)?;
                    if s == 0 {
                        return Err(MediaError::invalid("TimestampScale of zero"));
                    }
                    self.info.timestamp_scale = s;
                }
                id::DURATION => duration_units = Some(ebml::float(data)?),
                _ => {}
            }
        }
        if let Some(d) = duration_units.filter(|d| d.is_finite() && *d >= 0.0) {
            let ns = d * self.info.timestamp_scale as f64;
            let ns = if ns >= u64::MAX as f64 {
                u64::MAX
            } else {
                ns.round() as u64
            };
            // Zero means "not declared" (the player learns the length from the stream). Anything
            // else under a millisecond is no clip a player can show, and a looping player would
            // count one wrap per declared length on every tick (deep-zai-media finding 1).
            if ns != 0 && ns < MIN_DECLARED_DURATION_NS {
                return Err(MediaError::invalid(format!(
                    "declared Duration of {ns} ns is under {MIN_DECLARED_DURATION_NS} ns"
                )));
            }
            self.info.duration_ns = Some(ns);
        }
        Ok(())
    }

    fn parse_tracks(&mut self, body: &[u8]) -> Result<(), MediaError> {
        let mut c = Cursor::new(body);
        while !c.is_empty() {
            let (eid, data) = c.element()?;
            if eid != id::TRACK_ENTRY {
                continue;
            }
            if self.info.tracks.len() >= MAX_TRACKS {
                return Err(MediaError::LimitExceeded(format!(
                    "more than {MAX_TRACKS} tracks"
                )));
            }
            let index = self.info.tracks.len() as u32;
            let track = parse_track(data, index)?;
            self.info.tracks.push(track);
        }
        Ok(())
    }

    fn parse_cues(&mut self, body: &[u8]) -> Result<(), MediaError> {
        let mut c = Cursor::new(body);
        while !c.is_empty() {
            let (eid, data) = c.element()?;
            if eid != id::CUE_POINT || self.cues.len() >= MAX_CUES {
                continue;
            }
            let mut time = None;
            // First position in the cue point. Used when the file has no video track, and when a
            // position omits `CueTrack` (the previous tolerance).
            let mut fallback: Option<(Option<u64>, u64)> = None;
            // The position whose `CueTrack` is the video track. Matroska allows one
            // `CueTrackPositions` per track, in any order; the video one is not always first.
            let mut video_pos: Option<u64> = None;
            let video = self.info.video_track().map(|t| t.number);
            let mut p = Cursor::new(data);
            while !p.is_empty() {
                let (pid, pdata) = p.element()?;
                match pid {
                    id::CUE_TIME => time = Some(ebml::uint(pdata)?),
                    id::CUE_TRACK_POSITIONS => {
                        let mut cue_track = None;
                        let mut cue_pos = None;
                        let mut q = Cursor::new(pdata);
                        while !q.is_empty() {
                            let (qid, qdata) = q.element()?;
                            match qid {
                                id::CUE_TRACK => cue_track = Some(ebml::uint(qdata)?),
                                id::CUE_CLUSTER_POSITION => cue_pos = Some(ebml::uint(qdata)?),
                                _ => {}
                            }
                        }
                        let Some(cue_pos) = cue_pos else { continue };
                        if fallback.is_none() {
                            fallback = Some((cue_track, cue_pos));
                        }
                        if video.is_some() && cue_track == video && video_pos.is_none() {
                            video_pos = Some(cue_pos);
                        }
                    }
                    _ => {}
                }
            }
            let pos = if video.is_none() {
                fallback.map(|(_, pos)| pos)
            } else if let Some(pos) = video_pos {
                Some(pos)
            } else if matches!(fallback, Some((None, _))) {
                fallback.map(|(_, pos)| pos)
            } else {
                None
            };
            if let (Some(time), Some(pos)) = (time, pos) {
                let abs = self.seg_data.saturating_add(pos);
                if abs < self.seg_end {
                    let time_ns = self.scale(time.min(i64::MAX as u64) as i64).max(0) as u64;
                    self.cues.push(CuePoint { time_ns, pos: abs });
                }
            }
        }
        self.cues.sort_by_key(|c| c.time_ns);
        Ok(())
    }

    fn parse_chapters(&mut self, body: &[u8]) -> Result<(), MediaError> {
        let mut c = Cursor::new(body);
        while !c.is_empty() {
            let (eid, edition) = c.element()?;
            if eid != id::EDITION_ENTRY {
                continue;
            }
            let mut e = Cursor::new(edition);
            while !e.is_empty() {
                let (aid, atom) = e.element()?;
                if aid != id::CHAPTER_ATOM || self.info.markers.len() >= MAX_MARKERS {
                    continue;
                }
                let mut start = 0;
                let mut name = String::new();
                let mut a = Cursor::new(atom);
                while !a.is_empty() {
                    let (xid, x) = a.element()?;
                    match xid {
                        id::CHAPTER_TIME_START => start = ebml::uint(x)?,
                        id::CHAPTER_DISPLAY => {
                            let mut d = Cursor::new(x);
                            while !d.is_empty() {
                                let (did, dd) = d.element()?;
                                if did == id::CHAP_STRING && name.is_empty() {
                                    name = ebml::string(dd);
                                }
                            }
                        }
                        _ => {}
                    }
                }
                self.info.markers.push(Marker { name, time_ns: start });
            }
        }
        self.info.markers.sort_by_key(|m| m.time_ns);
        Ok(())
    }

    /// The next element header.
    ///
    /// `IoFailed` is returned to the caller. A vint that cannot be an element id
    /// (`0x00`, or longer than an id) resynchronizes at the next Cluster, the same way an
    /// implausible size does. Anything else (a header cut off by the end of the file) is a clean end.
    fn read_header(&mut self) -> Result<HeaderRead, MediaError> {
        if self.src.remaining() == 0 || self.src.pos() >= self.seg_end {
            return Ok(HeaderRead::End);
        }
        let at = self.src.pos();
        match self.src.header() {
            Ok(header) => Ok(HeaderRead::Header(header)),
            Err(MediaError::IoFailed(msg)) => Err(MediaError::IoFailed(msg)),
            Err(MediaError::ContainerInvalid(msg)) if msg == "bad variable-length integer" => {
                self.stats.skipped += 1;
                self.src.seek_to(at)?;
                if self.resync()? {
                    Ok(HeaderRead::Resynced)
                } else {
                    Ok(HeaderRead::End)
                }
            }
            Err(_) => Ok(HeaderRead::End),
        }
    }

    /// Scans for the next Cluster ID from the current offset (corruption recovery).
    fn resync(&mut self) -> Result<bool, MediaError> {
        self.stats.resyncs += 1;
        self.cluster = None;
        self.src.skip(1)?;
        self.src.scan_for(id::CLUSTER)
    }

    fn fill_pending(&mut self) -> Result<bool, MediaError> {
        loop {
            if self.src.pos() >= self.seg_end {
                return Ok(false);
            }
            let at = self.src.pos();
            let Some(cluster) = self.cluster else {
                // Between clusters: expect a level-1 element.
                let h = match self.read_header()? {
                    HeaderRead::End => return Ok(false),
                    HeaderRead::Resynced => continue,
                    HeaderRead::Header(header) => header,
                };
                if h.id == id::CLUSTER {
                    let end = h.size.map(|s| h.data_start.saturating_add(s));
                    if end.is_some_and(|e| e > self.src.len()) && self.src.remaining() == 0 {
                        return Ok(false);
                    }
                    self.cluster = Some(ClusterState { end, timestamp: 0 });
                    continue;
                }
                match h.size {
                    Some(s) if s <= self.src.remaining() => self.src.skip(s)?,
                    _ => {
                        self.stats.skipped += 1;
                        self.src.seek_to(at)?;
                        if !self.resync()? {
                            return Ok(false);
                        }
                    }
                }
                continue;
            };
            if cluster.end.is_some_and(|e| at >= e) {
                self.cluster = None;
                continue;
            }
            let h = match self.read_header()? {
                HeaderRead::End => return Ok(false), // truncated final cluster: end early, not an error
                HeaderRead::Resynced => continue,
                HeaderRead::Header(header) => header,
            };
            // An unknown-size cluster ends at the next level-1 element.
            if cluster.end.is_none()
                && matches!(
                    h.id,
                    id::CLUSTER | id::CUES | id::TAGS | id::CHAPTERS | id::INFO | id::TRACKS
                )
            {
                self.src.seek_to(at)?;
                self.cluster = None;
                continue;
            }
            let limit = cluster.end.unwrap_or(self.seg_end).min(self.src.len());
            let Some(size) = h.size.filter(|s| h.data_start.saturating_add(*s) <= limit) else {
                if h.data_start.saturating_add(h.size.unwrap_or(0)) > self.src.len()
                    && matches!(h.id, id::SIMPLE_BLOCK | id::BLOCK_GROUP)
                {
                    return Ok(false); // truncated last block
                }
                self.stats.skipped += 1;
                self.src.seek_to(at)?;
                if !self.resync()? {
                    return Ok(false);
                }
                continue;
            };
            match h.id {
                id::TIMESTAMP => {
                    let body = self.src.read_vec(size)?;
                    match ebml::uint(&body) {
                        Ok(raw) => {
                            self.cluster = Some(ClusterState {
                                timestamp: raw.min(i64::MAX as u64) as i64,
                                ..cluster
                            });
                        }
                        Err(_) => {
                            // Present, but not a uint. A missing Timestamp is the Matroska default
                            // of 0; this one must not publish the cluster's blocks at time 0.
                            self.stats.skipped += 1;
                            self.cluster = None;
                            if !self.resync()? {
                                return Ok(false);
                            }
                        }
                    }
                }
                id::SIMPLE_BLOCK => {
                    let body = self.src.read_vec(size)?;
                    if self.push_block(&body, cluster.timestamp, None, None).is_err() {
                        self.stats.skipped += 1;
                    }
                }
                id::BLOCK_GROUP => {
                    let body = self.src.read_vec(size)?;
                    if self.push_group(&body, cluster.timestamp).is_err() {
                        self.stats.skipped += 1;
                    }
                }
                _ => self.src.skip(size)?,
            }
            if !self.pending.is_empty() {
                return Ok(true);
            }
        }
    }

    fn push_group(&mut self, body: &[u8], cluster_ts: i64) -> Result<(), MediaError> {
        let mut c = Cursor::new(body);
        let mut block = None;
        let mut duration = None;
        let mut referenced = false;
        while !c.is_empty() {
            let (eid, data) = c.element()?;
            match eid {
                id::BLOCK => block = Some(data),
                id::BLOCK_DURATION => duration = Some(ebml::uint(data)?),
                id::REFERENCE_BLOCK => referenced = true,
                _ => {}
            }
        }
        let block = block.ok_or_else(|| MediaError::invalid("BlockGroup without a Block"))?;
        let dur_ns = duration.map(|d| self.scale(d.min(i64::MAX as u64) as i64).max(0) as u64);
        self.push_block(block, cluster_ts, Some(!referenced), dur_ns)
    }

    fn push_block(
        &mut self,
        body: &[u8],
        cluster_ts: i64,
        keyframe: Option<bool>,
        duration_ns: Option<u64>,
    ) -> Result<(), MediaError> {
        let mut c = Cursor::new(body);
        let track = c.vint()?.ok_or_else(|| MediaError::invalid("bad block track"))?;
        let rel = i16::from_be_bytes([c.byte()?, c.byte()?]);
        let flags = c.byte()?;
        let key = keyframe.unwrap_or(flags & 0x80 != 0);
        let pts_ns = self.scale(cluster_ts.saturating_add(i64::from(rel)));
        if flags & 0x30 != 0 {
            return Err(MediaError::invalid("encrypted or reserved block flags"));
        }
        let frames = read_laced(&mut c, (flags >> 1) & 0x03)?;
        let default_dur = self
            .info
            .tracks
            .iter()
            .find(|t| t.number == track)
            .and_then(|t| t.default_duration_ns);
        let n = frames.len();
        for (i, data) in frames.into_iter().enumerate() {
            let offset = lace_offset_ns(default_dur, i);
            let packet = Packet {
                track,
                pts_ns: pts_ns.saturating_add(offset),
                duration_ns: if n == 1 {
                    duration_ns.or(default_dur)
                } else {
                    default_dur
                },
                keyframe: key && i == 0,
                data,
            };
            if self.drop_until_key && Some(track) == self.video_track {
                if !packet.keyframe {
                    continue;
                }
                self.drop_until_key = false;
            }
            self.pending.push_back(packet);
        }
        Ok(())
    }
}

fn read_laced(c: &mut Cursor<'_>, lacing: u8) -> Result<Vec<Vec<u8>>, MediaError> {
    if lacing == 0 {
        let rest = c.remaining();
        return Ok(vec![c.take(rest)?.to_vec()]);
    }
    let count = usize::from(c.byte()?) + 1;
    if count > MAX_LACED {
        return Err(MediaError::invalid("too many laced frames"));
    }
    let mut sizes = Vec::with_capacity(count);
    match lacing {
        1 => {
            // Xiph lacing: sizes as runs of 255 plus a final byte.
            for _ in 0..count - 1 {
                let mut size = 0usize;
                loop {
                    let b = c.byte()?;
                    size += usize::from(b);
                    if b != 255 {
                        break;
                    }
                }
                sizes.push(size);
            }
        }
        3 => {
            // EBML lacing: first size, then signed differences.
            let first = c.vint()?.ok_or_else(|| MediaError::invalid("bad EBML lace"))?;
            let mut prev = i64::try_from(first).map_err(|_| MediaError::invalid("bad EBML lace"))?;
            sizes.push(prev as usize);
            for _ in 1..count - 1 {
                let start = c.remaining();
                let raw = c.vint()?.ok_or_else(|| MediaError::invalid("bad EBML lace"))?;
                let len = (start - c.remaining()) as u32;
                let bias = (1i64 << (7 * len - 1)) - 1;
                prev = prev
                    .checked_add(raw as i64 - bias)
                    .filter(|v| *v >= 0)
                    .ok_or_else(|| MediaError::invalid("bad EBML lace"))?;
                sizes.push(prev as usize);
            }
        }
        _ => {
            // Fixed-size lacing.
            let each = c.remaining() / count;
            if each * count != c.remaining() {
                return Err(MediaError::invalid("fixed lacing does not divide the block"));
            }
            sizes = vec![each; count - 1];
        }
    }
    let used: usize = sizes
        .iter()
        .try_fold(0usize, |a, s| a.checked_add(*s))
        .unwrap_or(usize::MAX);
    if used > c.remaining() {
        return Err(MediaError::invalid("laced sizes exceed the block"));
    }
    sizes.push(c.remaining() - used);
    sizes.iter().map(|s| c.take(*s).map(<[u8]>::to_vec)).collect()
}

fn parse_track(data: &[u8], index: u32) -> Result<TrackInfo, MediaError> {
    let mut c = Cursor::new(data);
    let mut t = TrackInfo {
        number: 0,
        index,
        kind: TrackKind::Other,
        codec_id: String::new(),
        codec_private: Vec::new(),
        language: "eng".to_string(),
        default: true,
        default_duration_ns: None,
        frame_rate: None,
        video: None,
        audio: None,
        verdict: CodecVerdict::Unknown,
    };
    while !c.is_empty() {
        let (eid, d) = c.element()?;
        match eid {
            id::TRACK_NUMBER => t.number = ebml::uint(d)?,
            id::TRACK_TYPE => {
                t.kind = match ebml::uint(d)? {
                    1 => TrackKind::Video,
                    2 => TrackKind::Audio,
                    17 | 33 => TrackKind::Text,
                    _ => TrackKind::Other,
                }
            }
            id::FLAG_DEFAULT => t.default = ebml::uint(d)? != 0,
            id::LANGUAGE => t.language = ebml::string(d),
            id::CODEC_ID => t.codec_id = ebml::string(d),
            id::CODEC_PRIVATE => t.codec_private = d.to_vec(),
            // Every later use reads the period as an `i64`, so a file's larger value counts as absent.
            id::DEFAULT_DURATION => {
                t.default_duration_ns = Some(ebml::uint(d)?).filter(|v| (1..=i64::MAX as u64).contains(v));
            }
            id::CONTENT_ENCODINGS => {
                return Err(MediaError::invalid(
                    "content encodings (compression or encryption) are refused",
                ));
            }
            id::VIDEO => t.video = Some(parse_video(d)?),
            id::AUDIO => t.audio = Some(parse_audio(d)?),
            _ => {}
        }
    }
    if t.number == 0 {
        return Err(MediaError::invalid("track without a number"));
    }
    t.frame_rate = t.default_duration_ns.map(|d| 1e9 / d as f64);
    t.verdict = codec::classify(&t.codec_id);
    Ok(t)
}

fn small(v: u64, what: &str) -> Result<u32, MediaError> {
    u32::try_from(v).map_err(|_| MediaError::LimitExceeded(format!("{what} {v}")))
}

/// An H.273 code point stored as a `u8`. Values that do not fit stay unspecified (2).
fn colour_code(bytes: &[u8]) -> Result<u8, MediaError> {
    Ok(colour_code_from(ebml::uint(bytes)?))
}

fn colour_code_from(val: u64) -> u8 {
    u8::try_from(val).unwrap_or(2)
}

fn parse_video(data: &[u8]) -> Result<VideoParams, MediaError> {
    let mut c = Cursor::new(data);
    let mut v = VideoParams {
        width: 0,
        height: 0,
        display_width: 0,
        display_height: 0,
        bit_depth: 8,
        chroma: ChromaFormat::Yuv420,
        color: ColorInfo::default(),
        fourcc: None,
    };
    let mut sub = (1u64, 1u64);
    while !c.is_empty() {
        let (eid, d) = c.element()?;
        match eid {
            id::PIXEL_WIDTH => v.width = small(ebml::uint(d)?, "width")?,
            id::PIXEL_HEIGHT => v.height = small(ebml::uint(d)?, "height")?,
            id::DISPLAY_WIDTH => v.display_width = small(ebml::uint(d)?, "display width")?,
            id::DISPLAY_HEIGHT => v.display_height = small(ebml::uint(d)?, "display height")?,
            id::UNCOMPRESSED_FOURCC => v.fourcc = Some(ebml::string(d)),
            id::COLOUR => {
                let mut k = Cursor::new(d);
                while !k.is_empty() {
                    let (kid, kd) = k.element()?;
                    // Match the id first. `MasteringMetadata` is a master (one chromaticity float
                    // is already more than 8 bytes). Reading every child as a uint refuses the file.
                    match kid {
                        id::MATRIX => v.color.matrix = colour_code(kd)?,
                        id::TRANSFER => v.color.transfer = colour_code(kd)?,
                        id::PRIMARIES => v.color.primaries = colour_code(kd)?,
                        id::RANGE => v.color.full_range = ebml::uint(kd)? == 2,
                        id::BITS_PER_CHANNEL => {
                            let val = ebml::uint(kd)?;
                            if val > 0 {
                                v.bit_depth = colour_code_from(val);
                            }
                        }
                        id::CHROMA_SUB_HORZ => sub.0 = ebml::uint(kd)?,
                        id::CHROMA_SUB_VERT => sub.1 = ebml::uint(kd)?,
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    v.chroma = match sub {
        (0, 0) => ChromaFormat::Yuv444,
        (1, 0) => ChromaFormat::Yuv422,
        _ => ChromaFormat::Yuv420,
    };
    if v.display_width == 0 {
        v.display_width = v.width;
    }
    if v.display_height == 0 {
        v.display_height = v.height;
    }
    Ok(v)
}

fn parse_audio(data: &[u8]) -> Result<AudioParams, MediaError> {
    let mut c = Cursor::new(data);
    let mut a = AudioParams::default();
    while !c.is_empty() {
        let (eid, d) = c.element()?;
        match eid {
            id::SAMPLING_FREQUENCY => a.sample_rate = ebml::float(d)?,
            id::CHANNELS => a.channels = u8::try_from(ebml::uint(d)?).unwrap_or(u8::MAX),
            id::BIT_DEPTH => a.bit_depth = u8::try_from(ebml::uint(d)?).unwrap_or(0),
            _ => {}
        }
    }
    if !(a.sample_rate.is_finite() && a.sample_rate > 0.0 && a.sample_rate <= 768_000.0) {
        return Err(MediaError::invalid("audio sample rate out of range"));
    }
    if a.channels == 0 || a.channels > MAX_AUDIO_CHANNELS {
        return Err(MediaError::invalid("audio channel count out of range"));
    }
    Ok(a)
}

impl<R: Read + Seek + Send> Demuxer for MkvDemuxer<R> {
    fn probe(&mut self) -> Result<MediaInfo, MediaError> {
        Ok(self.info.clone())
    }

    fn next_packet(&mut self) -> Result<Option<Packet>, MediaError> {
        if self.pending.is_empty() && !self.fill_pending()? {
            return Ok(None);
        }
        let packet = self.pending.pop_front();
        if packet.is_some() {
            self.stats.packets += 1;
        }
        Ok(packet)
    }

    fn seek(&mut self, to_ns: u64) -> Result<u64, MediaError> {
        self.pending.clear();
        self.cluster = None;
        self.drop_until_key = true;
        let idx = self.cues.partition_point(|c| c.time_ns <= to_ns);
        match idx.checked_sub(1).and_then(|i| self.cues.get(i)).copied() {
            Some(cue) => {
                self.src.seek_to(cue.pos)?;
                Ok(cue.time_ns)
            }
            None => {
                // No cue data (or target before the first cue): scan from the start.
                if self.cues.is_empty() {
                    tracing::warn!("video seek without cue data scans from the start");
                }
                self.rewind()?;
                Ok(0)
            }
        }
    }
}

/// Probes a file without opening a player (editor, agent, cook).
///
/// ```
/// use std::io::Cursor;
///
/// use regia_media::mux::PatternClip;
/// use regia_media::probe;
///
/// let info = probe(Cursor::new(PatternClip::new(32, 32, 6, 30).to_webm()))?;
/// assert_eq!(info.tracks.len(), 1);
/// // Bytes that are not a media file are an error, not a panic.
/// assert!(probe(Cursor::new(b"not a video".to_vec())).is_err());
/// # Ok::<(), regia_media::MediaError>(())
/// ```
pub fn probe(reader: impl Read + Seek + Send) -> Result<MediaInfo, MediaError> {
    MkvDemuxer::open(reader)?.probe()
}

#[cfg(test)]
mod lace_tests {
    use super::*;

    /// Media audit 2: a huge `DefaultDuration` cannot wrap a laced frame's time negative.
    #[test]
    fn lace_offsets_never_wrap() {
        assert_eq!(lace_offset_ns(Some(40), 3), 120);
        assert_eq!(lace_offset_ns(None, 3), 0);
        assert_eq!(lace_offset_ns(Some(1 << 63), 1), 0);
        assert_eq!(lace_offset_ns(Some(1 << 62), 4), 0);
    }
}

#[cfg(test)]
mod audit_g_tests {
    //! Regressions for the media correctness audit (sprint-audit-g-media-correctness).
    //! These live next to the demuxer, not in `tests/codecs.rs`: that file is compiled only when a
    //! compressed-codec feature is on, and the default feature set is empty.

    use std::io::{self, Read, Seek, SeekFrom};

    use super::*;
    use crate::ebml;
    use crate::error::MediaError;

    /// Matroska `Colour` / `MasteringMetadata` / `PrimaryRChromaticityX` (public element table).
    const MASTERING_METADATA: u32 = 0x55D0;
    const PRIMARY_R_CHROMATICITY_X: u32 = 0x55D1;

    fn el(out: &mut Vec<u8>, eid: u32, body: &[u8]) {
        ebml::write_element(out, eid, body);
    }

    fn uel(out: &mut Vec<u8>, eid: u32, value: u64) {
        el(out, eid, &ebml::uint_bytes(value));
    }

    fn simple_block(track: u64, payload: &[u8]) -> Vec<u8> {
        let mut block = Vec::with_capacity(payload.len() + 4);
        block.push(0x80 | u8::try_from(track).unwrap_or(1));
        block.extend_from_slice(&0i16.to_be_bytes());
        block.push(0x80);
        block.extend_from_slice(payload);
        block
    }

    fn cluster(timestamp: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut body = Vec::new();
        el(&mut body, id::TIMESTAMP, timestamp);
        el(&mut body, id::SIMPLE_BLOCK, &simple_block(1, payload));
        let mut out = Vec::new();
        el(&mut out, id::CLUSTER, &body);
        out
    }

    fn info_element() -> Vec<u8> {
        let mut body = Vec::new();
        uel(&mut body, id::TIMESTAMP_SCALE, 1_000_000);
        let mut out = Vec::new();
        el(&mut out, id::INFO, &body);
        out
    }

    fn video_entry(colour: Option<&[u8]>) -> Vec<u8> {
        let mut video = Vec::new();
        uel(&mut video, id::PIXEL_WIDTH, 16);
        uel(&mut video, id::PIXEL_HEIGHT, 16);
        if let Some(colour) = colour {
            el(&mut video, id::COLOUR, colour);
        }
        let mut entry = Vec::new();
        uel(&mut entry, id::TRACK_NUMBER, 1);
        uel(&mut entry, id::TRACK_TYPE, 1);
        el(&mut entry, id::CODEC_ID, b"V_UNCOMPRESSED");
        el(&mut entry, id::VIDEO, &video);
        entry
    }

    fn audio_entry() -> Vec<u8> {
        let mut audio = Vec::new();
        el(
            &mut audio,
            id::SAMPLING_FREQUENCY,
            &48_000.0f64.to_bits().to_be_bytes(),
        );
        uel(&mut audio, id::CHANNELS, 1);
        let mut entry = Vec::new();
        uel(&mut entry, id::TRACK_NUMBER, 2);
        uel(&mut entry, id::TRACK_TYPE, 2);
        el(&mut entry, id::CODEC_ID, b"A_PCM/FLOAT/IEEE");
        el(&mut entry, id::AUDIO, &audio);
        entry
    }

    fn tracks_element(entries: &[Vec<u8>]) -> Vec<u8> {
        let mut body = Vec::new();
        for entry in entries {
            el(&mut body, id::TRACK_ENTRY, entry);
        }
        let mut out = Vec::new();
        el(&mut out, id::TRACKS, &body);
        out
    }

    struct Built {
        bytes: Vec<u8>,
        /// Segment-relative offset of each cluster (a `CueClusterPosition`).
        cluster_pos: Vec<u64>,
        /// Absolute file offset of each cluster element.
        cluster_at: Vec<u64>,
    }

    /// `gap` is inserted before every cluster after the first (a `0x00` between clusters).
    fn build(prelude: &[u8], clusters: &[Vec<u8>], gap: &[u8], tail: &[u8]) -> Built {
        let mut body = Vec::new();
        body.extend_from_slice(prelude);
        let mut cluster_pos = Vec::new();
        for (index, cluster) in clusters.iter().enumerate() {
            if index > 0 {
                body.extend_from_slice(gap);
            }
            cluster_pos.push(body.len() as u64);
            body.extend_from_slice(cluster);
        }
        body.extend_from_slice(tail);

        let mut header = Vec::new();
        uel(&mut header, 0x4286, 1);
        uel(&mut header, 0x42F7, 1);
        uel(&mut header, 0x42F2, 4);
        uel(&mut header, 0x42F3, 8);
        el(&mut header, id::DOC_TYPE, b"webm");
        uel(&mut header, 0x4287, 4);
        uel(&mut header, 0x4285, 2);
        let mut bytes = Vec::new();
        el(&mut bytes, id::EBML, &header);
        let before = bytes.len();
        el(&mut bytes, id::SEGMENT, &body);
        let seg_data = (before + (bytes.len() - before - body.len())) as u64;
        let cluster_at = cluster_pos.iter().map(|pos| seg_data + pos).collect();
        Built {
            bytes,
            cluster_pos,
            cluster_at,
        }
    }

    fn cue_point(time_units: u64, positions: &[(u64, u64)]) -> Vec<u8> {
        let mut point = Vec::new();
        uel(&mut point, id::CUE_TIME, time_units);
        for (track, pos) in positions {
            let mut ctp = Vec::new();
            uel(&mut ctp, id::CUE_TRACK, *track);
            uel(&mut ctp, id::CUE_CLUSTER_POSITION, *pos);
            el(&mut point, id::CUE_TRACK_POSITIONS, &ctp);
        }
        let mut out = Vec::new();
        el(&mut out, id::CUE_POINT, &point);
        out
    }

    /// A reader whose length stays the original file, but whose second read that starts at
    /// `fail_at` returns an `io::Error`. The first read at that offset (the metadata walk) succeeds.
    struct FailOnSecondVisit {
        data: Vec<u8>,
        pos: u64,
        fail_at: u64,
        visits: u32,
    }

    impl Read for FailOnSecondVisit {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.pos == self.fail_at {
                self.visits += 1;
                if self.visits > 1 {
                    return Err(io::Error::other("injected read failure"));
                }
            }
            if self.pos >= self.data.len() as u64 {
                return Ok(0);
            }
            let start = self.pos as usize;
            let n = buf.len().min(self.data.len() - start);
            buf[..n].copy_from_slice(&self.data[start..start + n]);
            self.pos += n as u64;
            Ok(n)
        }
    }

    impl Seek for FailOnSecondVisit {
        fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
            let next = match from {
                SeekFrom::Start(pos) => pos,
                SeekFrom::End(delta) => (self.data.len() as i64 + delta).max(0) as u64,
                SeekFrom::Current(delta) => (self.pos as i64 + delta).max(0) as u64,
            };
            self.pos = next;
            Ok(self.pos)
        }
    }

    fn packets(bytes: Vec<u8>) -> Result<Vec<Packet>, MediaError> {
        let mut demux = MkvDemuxer::open(std::io::Cursor::new(bytes))?;
        let mut out = Vec::new();
        while let Some(packet) = demux.next_packet()? {
            out.push(packet);
        }
        Ok(out)
    }

    /// Finding 1: a `Colour` master that contains `MasteringMetadata` must still open, and the
    /// uint children (`MatrixCoefficients`) must still be stored.
    #[test]
    fn mastering_metadata_in_colour_does_not_refuse_the_file() {
        let mut chromaticity = Vec::new();
        el(
            &mut chromaticity,
            PRIMARY_R_CHROMATICITY_X,
            &0.68f64.to_bits().to_be_bytes(),
        );
        let mut colour = Vec::new();
        uel(&mut colour, id::MATRIX, 9);
        el(&mut colour, MASTERING_METADATA, &chromaticity);
        let prelude = {
            let mut prelude = info_element();
            prelude.extend(tracks_element(&[video_entry(Some(&colour))]));
            prelude
        };
        let built = build(&prelude, &[cluster(&ebml::uint_bytes(0), &[0x11])], &[], &[]);
        let demux = MkvDemuxer::open(std::io::Cursor::new(built.bytes)).expect("open");
        let video = demux
            .info()
            .video_track()
            .and_then(|track| track.video.as_ref())
            .expect("video");
        assert_eq!(video.color.matrix, 9);
    }

    /// Finding 3: a cue whose first `CueTrackPositions` is the audio track must still keep the
    /// video track's cluster offset, which is listed second.
    #[test]
    fn a_cue_keeps_the_video_position_when_audio_is_listed_first() {
        let clusters = [
            cluster(&ebml::uint_bytes(0), &[0x11]),
            cluster(&ebml::uint_bytes(1_000), &[0x22]),
        ];
        let prelude = {
            let mut prelude = info_element();
            prelude.extend(tracks_element(&[video_entry(None), audio_entry()]));
            prelude
        };
        // Positions are filled after the clusters are placed. Build once with an empty tail to
        // learn the offsets, then rebuild with the cue. Cluster bytes do not depend on the tail.
        let placed = build(&prelude, &clusters, &[], &[]);
        let mut cues = Vec::new();
        cues.extend(cue_point(
            1_000,
            &[(2, placed.cluster_pos[0]), (1, placed.cluster_pos[1])],
        ));
        let mut cues_el = Vec::new();
        el(&mut cues_el, id::CUES, &cues);
        let built = build(&prelude, &clusters, &[], &cues_el);
        assert_eq!(built.cluster_pos, placed.cluster_pos);
        let mut demux = MkvDemuxer::open(std::io::Cursor::new(built.bytes)).expect("open");
        let landed = demux.seek(1_000_000_000).expect("seek");
        assert_eq!(landed, 1_000_000_000, "the video cue is kept");
        let packet = demux.next_packet().expect("packet").expect("cluster packet");
        assert_eq!(packet.data, vec![0x22]);
        assert_eq!(packet.pts_ns, 1_000_000_000);
    }

    /// Finding 4: a read error after the first cluster, while the measured length still reaches
    /// the rest of the file, is `io_failed` and not a clean end.
    #[test]
    fn a_read_error_after_the_first_cluster_is_io_failed() {
        let prelude = {
            let mut prelude = info_element();
            prelude.extend(tracks_element(&[video_entry(None)]));
            prelude
        };
        let built = build(
            &prelude,
            &[
                cluster(&ebml::uint_bytes(0), &[0x11]),
                cluster(&ebml::uint_bytes(1_000), &[0x22]),
            ],
            &[],
            &[],
        );
        let reader = FailOnSecondVisit {
            data: built.bytes,
            pos: 0,
            fail_at: built.cluster_at[1],
            visits: 0,
        };
        let mut demux = MkvDemuxer::open(reader).expect("open walks the second cluster header once");
        let first = demux.next_packet().expect("first cluster").expect("packet");
        assert_eq!(first.data, vec![0x11]);
        let err = demux
            .next_packet()
            .expect_err("a mid-file read error is not the end of the clip");
        assert_eq!(err.code(), "io_failed", "{err}");
    }

    /// Finding 4: a `0x00` id byte between clusters is not a clean end while bytes remain.
    #[test]
    fn a_zero_byte_between_clusters_does_not_end_the_file() {
        let prelude = {
            let mut prelude = info_element();
            prelude.extend(tracks_element(&[video_entry(None)]));
            prelude
        };
        let built = build(
            &prelude,
            &[
                cluster(&ebml::uint_bytes(0), &[0x11]),
                cluster(&ebml::uint_bytes(1_000), &[0x22]),
            ],
            &[0x00],
            &[],
        );
        let mut demux = MkvDemuxer::open(std::io::Cursor::new(built.bytes)).expect("open");
        let first = demux.next_packet().expect("first").expect("first packet");
        assert_eq!(first.data, vec![0x11]);
        match demux.next_packet() {
            Ok(Some(packet)) => {
                assert_eq!(packet.data, vec![0x22]);
                assert_eq!(packet.pts_ns, 1_000_000_000);
            }
            Ok(None) => panic!("clean end while the second cluster is still unread"),
            Err(err) => assert_eq!(err.code(), "container_invalid", "{err}"),
        }
    }

    /// Finding 5: a Cluster `Timestamp` that is present but longer than a uint is not time 0.
    /// The cluster's blocks are skipped.
    #[test]
    fn a_cluster_timestamp_that_is_not_a_uint_is_skipped() {
        let prelude = {
            let mut prelude = info_element();
            prelude.extend(tracks_element(&[video_entry(None)]));
            prelude
        };
        let built = build(
            &prelude,
            &[cluster(&ebml::uint_bytes(5), &[0x11]), cluster(&[0; 9], &[0x22])],
            &[],
            &[],
        );
        let found = packets(built.bytes).expect("demux");
        assert!(
            found.iter().any(|packet| packet.data == [0x11]),
            "the good cluster is kept"
        );
        assert!(
            found
                .iter()
                .all(|packet| packet.data != [0x22] || packet.pts_ns != 0),
            "the bad cluster is not published at time 0: {found:?}"
        );
        assert!(
            found.iter().all(|packet| packet.data != [0x22]),
            "blocks after an unreadable Timestamp are skipped: {found:?}"
        );
    }
}

#[cfg(test)]
mod language_alias_tests {
    use super::languages_match;

    /// BCP-47 primaries that are not the ISO 639-1 code still name the same language as the
    /// Matroska tag (`nb`/`nn` for Norwegian, legacy `iw` and `in`). Bokmål does not match Nynorsk.
    #[test]
    fn bcp47_primaries_match_the_iso639_track_tag() {
        assert!(languages_match("nb-NO", "nor"), "nb-NO does not match nor");
        assert!(languages_match("nb", "nob"), "nb does not match nob");
        assert!(languages_match("nn-NO", "nno"), "nn-NO does not match nno");
        assert!(!languages_match("nb", "nn"), "bokmal is not nynorsk");
        assert!(!languages_match("nob", "nno"), "bokmal is not nynorsk");
        assert!(languages_match("no", "nor"), "the macrolanguage still matches");
        assert!(languages_match("iw-IL", "heb"), "legacy iw does not match heb");
        assert!(languages_match("in", "ind"), "legacy in does not match ind");
        assert!(languages_match("en-US", "eng"));
        assert!(!languages_match("und", "nor"));
    }
}
