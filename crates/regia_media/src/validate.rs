// SPDX-License-Identifier: MPL-2.0
//! Whole-file validation for import and cook (specs/50 § 4, ADR-2410): the checks a playing session
//! forgives but a shipped asset must not need. [`validate()`] refuses, each with its protocol code:
//!
//! - another container family (`container_unsupported`, by magic bytes),
//! - a bad EBML header or Segment, or any top-level element whose declared size runs past the end of
//!   the file: a truncated download or copy (`container_invalid`),
//! - malformed elements the demuxer had to skip or resynchronize over (`container_invalid`),
//! - a clip with no video track or no video packet (`container_invalid`),
//! - a patent-pooled codec on any video or audio track (`codec_patented`), an unknown video codec
//!   (`codec_unsupported`).
//!
//! A royalty-free codec without a decoder in this build (AV1 without the `video` feature) is
//! accepted: the cook validates the file, the game build decodes it. Nothing is decoded or
//! transcoded here; the pass reads every packet once, so its cost is one read of the file.

use std::io::{Read, Seek};

use serde::{Deserialize, Serialize};

use crate::codec::CodecVerdict;
use crate::container::{ContainerKind, Demuxer, MkvDemuxer, TrackKind, id};
use crate::ebml::Source;
use crate::error::MediaError;

/// Most packets the validation pass reads (a guard against hostile files of tiny clusters).
pub const MAX_VALIDATE_PACKETS: u64 = 50_000_000;

/// One track as the cook records it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClipTrack {
    /// Index in the track list.
    pub index: u32,
    /// `video`, `audio`, `text` or `other`.
    pub kind: String,
    /// Matroska codec id.
    pub codec_id: String,
    /// Language tag.
    pub language: String,
    /// Packets read for this track.
    pub packets: u64,
}

/// What [`validate()`] found: the metadata a `.meta`/cooked record keeps.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClipReport {
    /// `webm` or `matroska`.
    pub container: String,
    /// File size in bytes.
    pub bytes: u64,
    /// Declared duration in nanoseconds.
    pub duration_ns: Option<u64>,
    /// Coded width of the first video track.
    pub width: u32,
    /// Coded height of the first video track.
    pub height: u32,
    /// Nominal frame rate of the first video track.
    pub frame_rate: Option<f64>,
    /// Cue points.
    pub cue_count: u32,
    /// Chapter markers.
    pub markers: u32,
    /// Tracks in file order.
    pub tracks: Vec<ClipTrack>,
    /// Packets read in total.
    pub packets: u64,
    /// Time of the last packet in nanoseconds.
    pub last_packet_ns: i64,
}

fn kind_name(kind: TrackKind) -> &'static str {
    match kind {
        TrackKind::Video => "video",
        TrackKind::Audio => "audio",
        TrackKind::Text => "text",
        TrackKind::Other => "other",
    }
}

/// Checks that every top-level element of the file fits inside it.
fn check_structure<R: Read + Seek>(reader: &mut R) -> Result<u64, MediaError> {
    let mut src = Source::new(&mut *reader)?;
    let len = src.len();
    let ebml = src.header()?;
    if ebml.id != id::EBML {
        return Err(MediaError::invalid("not an EBML file"));
    }
    let ebml_size = ebml
        .size
        .ok_or_else(|| MediaError::invalid("EBML header of unknown size"))?;
    if ebml.data_start.saturating_add(ebml_size) > len {
        return Err(MediaError::invalid(
            "truncated file: the EBML header runs past the end",
        ));
    }
    src.skip(ebml_size)?;
    let seg = src.header()?;
    if seg.id != id::SEGMENT {
        return Err(MediaError::invalid("no Segment after the EBML header"));
    }
    let seg_end = match seg.size {
        Some(size) => {
            let end = seg.data_start.saturating_add(size);
            if end > len {
                return Err(MediaError::invalid(format!(
                    "truncated file: the Segment declares {} bytes but {} are present",
                    size,
                    len.saturating_sub(seg.data_start)
                )));
            }
            end
        }
        None => len,
    };
    while src.pos() < seg_end {
        let start = src.pos();
        let h = src.header().map_err(|_| {
            MediaError::invalid(format!(
                "truncated file: element header at byte {start} is cut off"
            ))
        })?;
        let Some(size) = h.size else {
            // An unknown-size element (a live-written Cluster) runs to the end of the Segment.
            break;
        };
        if h.data_start.saturating_add(size) > seg_end {
            let what = match h.id {
                id::CLUSTER => "a Cluster",
                id::CUES => "the Cues",
                id::TRACKS => "the Tracks",
                _ => "an element",
            };
            return Err(MediaError::invalid(format!(
                "truncated file: {what} at byte {start} declares {size} bytes, {} remain",
                seg_end.saturating_sub(h.data_start)
            )));
        }
        src.skip(size)?;
    }
    Ok(len)
}

/// Validates a whole media file (see the module docs). The reader is rewound as needed.
///
/// ```
/// use std::io::Cursor;
///
/// use regia_media::mux::PatternClip;
/// use regia_media::validate;
///
/// let report = validate(Cursor::new(PatternClip::new(64, 48, 8, 30).to_webm()))?;
/// assert_eq!((report.width, report.height), (64, 48));
/// assert_eq!(report.tracks.len(), 1);
///
/// // A file cut short is refused with a typed error.
/// let mut bytes = PatternClip::new(64, 48, 8, 30).to_webm();
/// bytes.truncate(bytes.len() / 2);
/// assert!(validate(Cursor::new(bytes)).is_err());
/// # Ok::<(), regia_media::MediaError>(())
/// ```
pub fn validate<R: Read + Seek + Send>(mut reader: R) -> Result<ClipReport, MediaError> {
    let _span = tracing::debug_span!("video_validate").entered();
    // The demuxer sniffs foreign containers first (`container_unsupported`).
    let bytes = {
        MkvDemuxer::open(&mut reader)?;
        check_structure(&mut reader)?
    };
    reader.seek(std::io::SeekFrom::Start(0)).map_err(MediaError::io)?;
    let mut demux = MkvDemuxer::open(&mut reader)?;
    let info = demux.probe()?;
    for t in &info.tracks {
        if matches!(t.kind, TrackKind::Video | TrackKind::Audio) && t.verdict == CodecVerdict::Patented {
            return Err(MediaError::CodecPatented(t.codec_id.clone()));
        }
    }
    let video = info
        .video_track()
        .ok_or_else(|| MediaError::invalid("no video track"))?
        .clone();
    if video.verdict == CodecVerdict::Unknown {
        return Err(MediaError::CodecUnsupported {
            codec: video.codec_id.clone(),
            hint: ": use AV1 in WebM".to_string(),
        });
    }
    let mut tracks: Vec<ClipTrack> = info
        .tracks
        .iter()
        .map(|t| ClipTrack {
            index: t.index,
            kind: kind_name(t.kind).to_string(),
            codec_id: t.codec_id.clone(),
            language: t.language.clone(),
            packets: 0,
        })
        .collect();
    let mut packets = 0u64;
    let mut last = 0i64;
    while let Some(p) = demux.next_packet()? {
        packets += 1;
        if packets > MAX_VALIDATE_PACKETS {
            return Err(MediaError::LimitExceeded(format!(
                "more than {MAX_VALIDATE_PACKETS} packets"
            )));
        }
        last = last.max(p.pts_ns);
        if let Some(i) = info.tracks.iter().position(|t| t.number == p.track)
            && let Some(t) = tracks.get_mut(i)
        {
            t.packets += 1;
        }
    }
    let stats = demux.stats();
    if stats.skipped > 0 || stats.resyncs > 0 {
        return Err(MediaError::invalid(format!(
            "{} malformed elements skipped, {} resynchronizations",
            stats.skipped, stats.resyncs
        )));
    }
    let video_packets = info
        .tracks
        .iter()
        .position(|t| t.number == video.number)
        .and_then(|i| tracks.get(i))
        .map_or(0, |t| t.packets);
    if video_packets == 0 {
        return Err(MediaError::invalid("no video packet"));
    }
    let v = video.video.as_ref();
    Ok(ClipReport {
        container: match info.container {
            ContainerKind::Webm => "webm",
            ContainerKind::Matroska => "matroska",
        }
        .to_string(),
        bytes,
        duration_ns: info.duration_ns,
        width: v.map_or(0, |v| v.width),
        height: v.map_or(0, |v| v.height),
        frame_rate: video.frame_rate,
        cue_count: info.cue_count.min(u32::MAX as usize) as u32,
        markers: info.markers.len() as u32,
        tracks,
        packets,
        last_packet_ns: last,
    })
}

/// Magic of a cooked video payload (ADR-2410).
pub const COOKED_VIDEO_MAGIC: [u8; 8] = *b"RGVIDEO\0";
/// Format version of the cooked video payload.
pub const COOKED_VIDEO_VERSION: u32 = 1;
/// The media bytes of a cooked video start at this offset, so a pak that aligns payloads to 4 KiB
/// keeps the file data 4 KiB aligned (specs/50 § 4).
pub const COOKED_VIDEO_DATA_OFFSET: usize = 4096;

/// A cooked video payload: the [`ClipReport`] (RON) and the media file byte for byte, never
/// transcoded. Layout (little endian): magic (8), version `u32`, report length `u32`, report RON,
/// zero padding to [`COOKED_VIDEO_DATA_OFFSET`], the file bytes.
pub fn encode_cooked_video(report: &ClipReport, file: &[u8]) -> Result<Vec<u8>, MediaError> {
    let meta = ron::to_string(report).map_err(|e| MediaError::invalid(format!("clip report: {e}")))?;
    let head = COOKED_VIDEO_MAGIC.len() + 8 + meta.len();
    if head > COOKED_VIDEO_DATA_OFFSET {
        return Err(MediaError::LimitExceeded(format!(
            "clip report of {} bytes does not fit the cooked header",
            meta.len()
        )));
    }
    let mut out = Vec::with_capacity(COOKED_VIDEO_DATA_OFFSET + file.len());
    out.extend_from_slice(&COOKED_VIDEO_MAGIC);
    out.extend_from_slice(&COOKED_VIDEO_VERSION.to_le_bytes());
    out.extend_from_slice(&(meta.len() as u32).to_le_bytes());
    out.extend_from_slice(meta.as_bytes());
    out.resize(COOKED_VIDEO_DATA_OFFSET, 0);
    out.extend_from_slice(file);
    Ok(out)
}

/// Reads a cooked video payload back: the report and the media bytes. A wrong magic, a newer
/// version or a damaged header is `container_invalid` naming the problem.
pub fn decode_cooked_video(payload: &[u8]) -> Result<(ClipReport, &[u8]), MediaError> {
    let bad = |why: &str| MediaError::invalid(format!("cooked video: {why}"));
    if payload.len() < COOKED_VIDEO_DATA_OFFSET || payload[..8] != COOKED_VIDEO_MAGIC {
        return Err(bad("not a cooked video"));
    }
    let word =
        |at: usize| u32::from_le_bytes([payload[at], payload[at + 1], payload[at + 2], payload[at + 3]]);
    let version = word(8);
    if version != COOKED_VIDEO_VERSION {
        return Err(bad(&format!(
            "version {version}, this build reads {COOKED_VIDEO_VERSION}"
        )));
    }
    let len = word(12) as usize;
    let meta = payload
        .get(16..16 + len)
        .filter(|_| 16 + len <= COOKED_VIDEO_DATA_OFFSET)
        .ok_or_else(|| bad("report length out of range"))?;
    let text = std::str::from_utf8(meta).map_err(|_| bad("report is not UTF-8"))?;
    let report: ClipReport = ron::from_str(text).map_err(|e| bad(&e.to_string()))?;
    Ok((report, &payload[COOKED_VIDEO_DATA_OFFSET..]))
}

/// The serde shape of the cooked video payload's clip report, for `cargo xtask check-formats`.
pub fn cooked_video_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names};
    join_field_lists(&[
        serde_field_names::<ClipReport>(),
        serde_field_names::<ClipTrack>(),
    ])
}

/// Versions of the cooked video payload a migrator reads (none: version 1 is the first).
pub fn cooked_video_migration_sources() -> Vec<u32> {
    Vec::new()
}
