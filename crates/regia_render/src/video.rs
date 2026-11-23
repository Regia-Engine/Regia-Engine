// SPDX-License-Identifier: MPL-2.0
//! Video frames for the renderer (specs/50 § 2.7, ADR-2044): the [`VideoFrameSource`] seam through
//! which the UI ([`VideoFrames`] resource) and materials read a video player's current picture, the
//! transport [`VideoControl`]s a widget or an agent sends back, the YUV to RGB conversion constants
//! shared by the CPU fast path ([`yuv_to_rgba8`]) and the GPU convert pass
//! (`crate::video_convert`), and the material helper [`video_material`].
//!
//! `regia_render` never names the media crate: `regia_media` implements [`VideoFrameSource`] and
//! installs it in [`VideoFrames`]; `regia_ui` reads it from there. Nothing here runs unless a host
//! installs a source, so frames without video are unchanged.
//!
//! Conversion follows ITU-R BT.601-7, BT.709-6 and BT.2020-2 with ITU-T H.273 code points (see
//! `docs/provenance.md`): limited range maps luma `16..235` and chroma `16..240` (scaled by the bit
//! depth) to `0..1` and `-0.5..0.5`; the result is the encoded R'G'B' value a sRGB view displays.
//! Chroma is upsampled by nearest sample (co-sited left), as the media crate's CPU reference does.

use std::sync::Arc;

use regia_ecs::prelude::*;

use crate::lab_frame::LabMaterial;
use crate::texture_residency::{DynamicTexture, MaterialImage};

/// A YUV to RGB matrix (H.273 `matrix_coefficients`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum YuvMatrix {
    /// ITU-R BT.601 (codes 5 and 6).
    Bt601,
    /// ITU-R BT.709 (code 1, and the default for unspecified or unknown codes).
    #[default]
    Bt709,
    /// ITU-R BT.2020 non-constant luminance (code 9).
    Bt2020Ncl,
}

impl YuvMatrix {
    /// The matrix for an H.273 code; unknown codes use BT.709.
    #[must_use]
    pub fn from_h273(code: u8) -> Self {
        match code {
            5 | 6 => Self::Bt601,
            9 => Self::Bt2020Ncl,
            _ => Self::Bt709,
        }
    }

    /// Luma weights `(Kr, Kb)`.
    #[must_use]
    pub fn kr_kb(self) -> (f64, f64) {
        match self {
            Self::Bt601 => (0.299, 0.114),
            Self::Bt709 => (0.2126, 0.0722),
            Self::Bt2020Ncl => (0.2627, 0.0593),
        }
    }
}

/// The affine map from raw `(Y, U, V, 1)` samples to encoded R'G'B' in `0..255` units: one row per
/// output channel. Built once per clip by [`convert_rows`]; the GPU pass uses it as three `vec4`s.
pub type ConvertRows = [[f64; 4]; 3];

/// The conversion rows for `bit_depth` (8..=16) samples.
#[must_use]
pub fn convert_rows(matrix: YuvMatrix, full_range: bool, bit_depth: u8) -> ConvertRows {
    let depth = i32::from(bit_depth.clamp(8, 16));
    let (kr, kb) = matrix.kr_kb();
    let kg = 1.0 - kr - kb;
    // yn = sy * Y + oy; cb = sc * U + oc; cr = sc * V + oc.
    let (sy, oy, sc, oc) = if full_range {
        let max = f64::from((1u32 << depth) - 1);
        let mid = f64::from(1u32 << (depth - 1));
        (1.0 / max, 0.0, 1.0 / max, -mid / max)
    } else {
        let scale = f64::from(1u32 << (depth - 8));
        (
            1.0 / (219.0 * scale),
            -16.0 / 219.0,
            1.0 / (224.0 * scale),
            -128.0 / 224.0,
        )
    };
    let rv = 2.0 * (1.0 - kr);
    let bu = 2.0 * (1.0 - kb);
    let gu = -kb * bu / kg;
    let gv = -kr * rv / kg;
    let row = |u: f64, v: f64| {
        [
            255.0 * sy,
            255.0 * u * sc,
            255.0 * v * sc,
            255.0 * (oy + (u + v) * oc),
        ]
    };
    [row(0.0, rv), row(gu, gv), row(bu, 0.0)]
}

/// One decoded picture's planes as the renderer sees them: planar YUV, tightly packed rows, 8-bit
/// samples (one byte) or 9..16-bit samples (two bytes, little endian).
#[derive(Debug, Clone, Copy)]
pub struct YuvPlanes<'a> {
    /// Luma width in pixels.
    pub width: u32,
    /// Luma height in pixels.
    pub height: u32,
    /// Bits per sample (8..=16).
    pub bit_depth: u8,
    /// Chroma subsampling shifts `(x, y)`: `(1, 1)` 4:2:0, `(1, 0)` 4:2:2, `(0, 0)` 4:4:4.
    pub chroma_shift: (u32, u32),
    /// Matrix.
    pub matrix: YuvMatrix,
    /// Full range instead of limited range.
    pub full_range: bool,
    /// Y, U, V.
    pub planes: [&'a [u8]; 3],
}

impl YuvPlanes<'_> {
    /// Bytes per sample.
    #[must_use]
    pub fn sample_bytes(&self) -> usize {
        if self.bit_depth > 8 { 2 } else { 1 }
    }

    /// Size of a chroma plane (rounded up). A shift of 32 or more (malformed decoder output)
    /// gives one sample per axis instead of overflowing; [`Self::is_complete`] refuses it.
    #[must_use]
    pub fn chroma_size(&self) -> (u32, u32) {
        let step = |shift: u32| 1u32.checked_shl(shift).unwrap_or(u32::MAX);
        (
            self.width.div_ceil(step(self.chroma_shift.0)),
            self.height.div_ceil(step(self.chroma_shift.1)),
        )
    }

    /// Whether every plane holds at least a full image of samples (and the chroma shifts are
    /// below 32, so the converters' `x >> shift` is defined).
    #[must_use]
    pub fn is_complete(&self) -> bool {
        let sb = self.sample_bytes();
        let (cw, ch) = self.chroma_size();
        let luma = self.width as usize * self.height as usize * sb;
        let chroma = cw as usize * ch as usize * sb;
        self.width > 0
            && self.height > 0
            && self.chroma_shift.0 < 32
            && self.chroma_shift.1 < 32
            && self.planes[0].len() >= luma
            && self.planes[1].len() >= chroma
            && self.planes[2].len() >= chroma
    }

    fn sample(&self, p: usize, x: u32, y: u32, w: u32) -> i64 {
        let i = (y as usize * w as usize + x as usize) * self.sample_bytes();
        let plane = self.planes[p];
        if self.sample_bytes() == 2 {
            match (plane.get(i), plane.get(i + 1)) {
                (Some(lo), Some(hi)) => i64::from(u16::from_le_bytes([*lo, *hi])),
                _ => 0,
            }
        } else {
            plane.get(i).map_or(0, |v| i64::from(*v))
        }
    }
}

/// Fixed-point fraction bits of the CPU fast path.
const FRAC: u32 = 16;

/// Converts `planes` to tightly packed RGBA8 (alpha 255) into `out` with fixed-point integer math:
/// within ±1 of the double-precision reference on every sample (the rows are rounded to 1/65536).
/// Returns `false` (and leaves `out` empty) when the planes are shorter than the size says.
pub fn yuv_to_rgba8(planes: &YuvPlanes<'_>, out: &mut Vec<u8>) -> bool {
    out.clear();
    if !planes.is_complete() {
        return false;
    }
    let _span = tracing::trace_span!("video_convert_cpu").entered();
    let rows = convert_rows(planes.matrix, planes.full_range, planes.bit_depth);
    let one = f64::from(1u32 << FRAC);
    let fixed = rows.map(|r| r.map(|c| (c * one).round() as i64));
    let half = 1i64 << (FRAC - 1);
    let (sx, sy) = planes.chroma_shift;
    let (cw, _) = planes.chroma_size();
    out.reserve(planes.width as usize * planes.height as usize * 4);
    for y in 0..planes.height {
        for x in 0..planes.width {
            let yy = planes.sample(0, x, y, planes.width);
            let u = planes.sample(1, x >> sx, y >> sy, cw);
            let v = planes.sample(2, x >> sx, y >> sy, cw);
            for r in &fixed {
                let c = (r[0] * yy + r[1] * u + r[2] * v + r[3] + half) >> FRAC;
                out.push(c.clamp(0, 255) as u8);
            }
            out.push(255);
        }
    }
    true
}

/// What a player is doing, as the renderer side sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VideoPlayback {
    /// No session.
    #[default]
    Closed,
    /// Waiting for the first picture.
    Opening,
    /// First picture shown, not playing.
    Ready,
    /// Playing.
    Playing,
    /// Paused.
    Paused,
    /// Catching up to a seek target.
    Seeking,
    /// Starved of pictures.
    Buffering,
    /// Ended (loop off).
    Ended,
    /// Failed.
    Failed,
}

impl VideoPlayback {
    /// Lowercase name (agent surface).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Closed => "closed",
            Self::Opening => "opening",
            Self::Ready => "ready",
            Self::Playing => "playing",
            Self::Paused => "paused",
            Self::Seeking => "seeking",
            Self::Buffering => "buffering",
            Self::Ended => "ended",
            Self::Failed => "failed",
        }
    }
}

/// The current picture of one player.
#[derive(Debug, Clone, PartialEq)]
pub struct VideoFrameInfo {
    /// Texture width (the coded display size, or 1 while only the fallback exists).
    pub width: u32,
    /// Texture height.
    pub height: u32,
    /// Display aspect (width / height) for layout.
    pub aspect: f32,
    /// Bumps every time the texture contents change.
    pub serial: u64,
    /// Presentation time of the shown picture in nanoseconds (`None` while the fallback shows).
    pub pts_ns: Option<i64>,
    /// The texture shows the fallback frame (the clip's clear color): no picture yet, closed,
    /// failed, or decoding stalled past the stall limit.
    pub fallback: bool,
    /// Playback state.
    pub playback: VideoPlayback,
    /// Position in seconds.
    pub position: f64,
    /// Duration in seconds, if known.
    pub duration: Option<f64>,
    /// Looping.
    pub looping: bool,
    /// The clock follows the audio output.
    pub audio_clock: bool,
}

/// A transport request from a widget, a script or an agent. Queued by the source and applied by
/// the media crate before the next clock tick, so a widget never blocks.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum VideoControl {
    /// Play (from `Ended` restarts at 0).
    Play,
    /// Pause.
    Pause,
    /// Play when paused, pause when playing.
    Toggle,
    /// Seek to seconds (accurate: the first picture at or after the target).
    Seek(f64),
    /// Loop forever (`true`) or play once (`false`).
    SetLoop(bool),
    /// Skip: close the player and report `Ended { skipped: true }` (refused with
    /// `video_not_skippable` for a fullscreen player marked not skippable).
    Skip,
}

/// Why a control was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VideoControlError {
    /// No player with that id.
    PlayerNotFound(u64),
    /// The control queue of the player is full (more than [`VIDEO_CONTROL_QUEUE`] pending).
    QueueFull(u64),
}

impl VideoControlError {
    /// Protocol code (`player_not_found`, `control_queue_full`).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::PlayerNotFound(_) => "player_not_found",
            Self::QueueFull(_) => "control_queue_full",
        }
    }
}

impl std::fmt::Display for VideoControlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PlayerNotFound(p) => write!(f, "no video player {p}"),
            Self::QueueFull(p) => write!(f, "video player {p} has {VIDEO_CONTROL_QUEUE} controls pending"),
        }
    }
}

impl std::error::Error for VideoControlError {}

/// Most controls a player holds between two ticks.
pub const VIDEO_CONTROL_QUEUE: usize = 32;

/// Where the UI and materials read video pictures from (specs/50 § 3, implemented by the media
/// crate). Players are addressed by their entity bits.
pub trait VideoFrameSource: Send + Sync + 'static {
    /// The current picture of `player`.
    fn frame(&self, player: u64) -> Option<VideoFrameInfo>;
    /// The texture for `player`'s picture (sRGB RGBA8). The handle is stable while the size stays
    /// the same. The media crate writes the picture into it only while a `VideoMaterial` binds the
    /// player; without one the texture may hold an older picture or none. Read [`Self::rgba`] for
    /// the current picture.
    fn texture(&self, player: u64) -> Option<DynamicTexture>;
    /// The picture as tightly packed sRGB RGBA8 bytes (`width * height * 4`), with its serial.
    fn rgba(&self, player: u64) -> Option<(u64, u32, u32, Arc<[u8]>)>;
    /// Queues a transport request for the next tick.
    ///
    /// # Errors
    /// [`VideoControlError`] when the player is unknown or its queue is full.
    fn control(&self, player: u64, control: VideoControl) -> Result<(), VideoControlError>;
    /// Every player the source knows, sorted.
    fn players(&self) -> Vec<u64>;
}

/// The installed [`VideoFrameSource`] (none by default: no video work happens).
#[derive(Resource, Clone, Default)]
pub struct VideoFrames(pub Option<Arc<dyn VideoFrameSource>>);

impl std::fmt::Debug for VideoFrames {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoFrames")
            .field("installed", &self.0.is_some())
            .finish()
    }
}

impl VideoFrames {
    /// The source, if one is installed.
    #[must_use]
    pub fn source(&self) -> Option<&dyn VideoFrameSource> {
        self.0.as_deref()
    }
}

/// Schedule sets of the video systems, so the media crate (which publishes pictures) and the UI
/// (which shows them) order against each other without naming each other.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VideoSystems {
    /// `Update`: queued controls are applied to the players (before the clock ticks).
    ApplyControls,
    /// `PostUpdate`: newly presented pictures are written to the player textures.
    Publish,
}

/// Which material slot a video binds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VideoSlot {
    /// Base color (lit like any texture).
    Albedo,
    /// Emissive (a self-lit screen; the material's emissive color and intensity scale it).
    #[default]
    Emissive,
}

impl VideoSlot {
    /// Parses `albedo` or `emissive`.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "albedo" | "base_color" => Some(Self::Albedo),
            "emissive" => Some(Self::Emissive),
            _ => None,
        }
    }

    /// Lowercase name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Albedo => "albedo",
            Self::Emissive => "emissive",
        }
    }
}

/// `base` with its `video` parameter bound: `slot` samples `texture` (the material's other
/// settings are kept). An emissive binding with a black emissive color gets white at intensity 1
/// so the screen shows.
#[must_use]
pub fn video_material(base: &LabMaterial, texture: &DynamicTexture, slot: VideoSlot) -> LabMaterial {
    let mut m = base.clone();
    let image = Some(MaterialImage::Dynamic(texture.clone()));
    match slot {
        VideoSlot::Albedo => m.images.albedo = image,
        VideoSlot::Emissive => {
            m.images.emissive = image;
            if m.emissive.iter().all(|c| *c <= 0.0) {
                m.emissive = [1.0, 1.0, 1.0];
            }
            if m.emissive_intensity <= 0.0 {
                m.emissive_intensity = 1.0;
            }
        }
    }
    m
}

/// Whether `material` binds `texture` in `slot` (the check behind `material_missing_video_slot`).
#[must_use]
pub fn binds_video(material: &LabMaterial, texture: &DynamicTexture, slot: VideoSlot) -> bool {
    let image = match slot {
        VideoSlot::Albedo => material.images.albedo.as_ref(),
        VideoSlot::Emissive => material.images.emissive.as_ref(),
    };
    matches!(image, Some(MaterialImage::Dynamic(t)) if t.identity() == texture.identity()
        && t.generation() == texture.generation())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The double-precision reference (the same math as the media crate's CPU reference).
    fn reference(y: u16, u: u16, v: u16, depth: u8, m: YuvMatrix, full: bool) -> [u8; 3] {
        let d = i32::from(depth);
        let scale = f64::from(1u32 << (d - 8));
        let (yn, cb, cr) = if full {
            let max = f64::from((1u32 << d) - 1);
            let mid = f64::from(1u32 << (d - 1));
            (
                f64::from(y) / max,
                (f64::from(u) - mid) / max,
                (f64::from(v) - mid) / max,
            )
        } else {
            (
                (f64::from(y) - 16.0 * scale) / (219.0 * scale),
                (f64::from(u) - 128.0 * scale) / (224.0 * scale),
                (f64::from(v) - 128.0 * scale) / (224.0 * scale),
            )
        };
        let (kr, kb) = m.kr_kb();
        let r = yn + 2.0 * (1.0 - kr) * cr;
        let b = yn + 2.0 * (1.0 - kb) * cb;
        let g = (yn - kr * r - kb * b) / (1.0 - kr - kb);
        let q = |c: f64| (c * 255.0).round().clamp(0.0, 255.0) as u8;
        [q(r), q(g), q(b)]
    }

    fn lcg(seed: &mut u64) -> u32 {
        *seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (*seed >> 33) as u32
    }

    /// The fixed-point fast path stays within ±1 of the reference for random samples of every
    /// matrix, range and depth.
    #[test]
    fn fast_path_matches_the_reference_within_one() {
        let mut seed = 7u64;
        for m in [YuvMatrix::Bt601, YuvMatrix::Bt709, YuvMatrix::Bt2020Ncl] {
            for full in [false, true] {
                for depth in [8u8, 10, 12] {
                    let max = (1u32 << depth) - 1;
                    for _ in 0..2000 {
                        let (y, u, v) = (
                            (lcg(&mut seed) % (max + 1)) as u16,
                            (lcg(&mut seed) % (max + 1)) as u16,
                            (lcg(&mut seed) % (max + 1)) as u16,
                        );
                        let sb = if depth > 8 { 2 } else { 1 };
                        let bytes = |s: u16| {
                            if sb == 2 {
                                s.to_le_bytes().to_vec()
                            } else {
                                vec![s as u8]
                            }
                        };
                        let (py, pu, pv) = (bytes(y), bytes(u), bytes(v));
                        let planes = YuvPlanes {
                            width: 1,
                            height: 1,
                            bit_depth: depth,
                            chroma_shift: (0, 0),
                            matrix: m,
                            full_range: full,
                            planes: [&py, &pu, &pv],
                        };
                        let mut out = Vec::new();
                        assert!(yuv_to_rgba8(&planes, &mut out));
                        let want = reference(y, u, v, depth, m, full);
                        for c in 0..3 {
                            let d = (i32::from(out[c]) - i32::from(want[c])).abs();
                            assert!(d <= 1, "{m:?} full={full} depth={depth} yuv=({y},{u},{v}) c={c}");
                        }
                        assert_eq!(out[3], 255);
                    }
                }
            }
        }
    }

    #[test]
    fn short_planes_are_refused() {
        let y = [0u8; 15];
        let c = [128u8; 4];
        let planes = YuvPlanes {
            width: 4,
            height: 4,
            bit_depth: 8,
            chroma_shift: (1, 1),
            matrix: YuvMatrix::Bt709,
            full_range: false,
            planes: [&y, &c, &c],
        };
        let mut out = vec![1];
        assert!(!yuv_to_rgba8(&planes, &mut out));
        assert!(out.is_empty());
    }

    #[test]
    fn subsampled_chroma_uses_the_co_sited_sample() {
        // 2x2 luma, one chroma sample: all four pixels share it.
        let y = [16u8, 235, 16, 235];
        let (u, v) = ([128u8], [128u8]);
        let planes = YuvPlanes {
            width: 2,
            height: 2,
            bit_depth: 8,
            chroma_shift: (1, 1),
            matrix: YuvMatrix::Bt709,
            full_range: false,
            planes: [&y, &u, &v],
        };
        let mut out = Vec::new();
        assert!(yuv_to_rgba8(&planes, &mut out));
        assert_eq!(&out[0..4], &[0, 0, 0, 255]);
        assert_eq!(&out[4..8], &[255, 255, 255, 255]);
    }

    #[test]
    fn video_material_binds_the_slot() {
        let tex = DynamicTexture::new(4, 2, true);
        let m = video_material(&LabMaterial::EMPTY, &tex, VideoSlot::Emissive);
        assert!(binds_video(&m, &tex, VideoSlot::Emissive));
        assert!(!binds_video(&m, &tex, VideoSlot::Albedo));
        assert!(m.emissive_intensity > 0.0 && m.emissive.iter().all(|c| *c > 0.0));
        let other = DynamicTexture::new(4, 2, true);
        assert!(!binds_video(&m, &other, VideoSlot::Emissive));
        let a = video_material(&LabMaterial::EMPTY, &tex, VideoSlot::Albedo);
        assert!(binds_video(&a, &tex, VideoSlot::Albedo));
        assert_eq!(VideoSlot::parse("emissive"), Some(VideoSlot::Emissive));
        assert_eq!(VideoSlot::parse("normal"), None);
    }
}
