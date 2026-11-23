// SPDX-License-Identifier: MPL-2.0
//! Offline video encoding to WebM: AV1 pictures through `rav1e` and Opus sound through
//! `unsafe-libopus`, muxed by Regia's own WebM writer ([`crate::mux::WebmWriter`]). Spec 50 § 14,
//! ADR-3369.
//!
//! Encoding is for render-out and tools only (ADR-1593 § 1): `render.out` format `webm` (the agent surface is
//! `render.out_start`) and the `regia media encode` command. A shipped game never encodes, so
//! the encoders sit behind the off-by-default `media-encode` feature. Without it everything here
//! still compiles: the settings, their checks, the color conversion and the input readers are plain
//! Rust, and [`available`] is `false`, so a caller answers [`UNAVAILABLE`] (`media_encode_unavailable`).
//!
//! The pieces:
//!
//! - [`EncodeSettings`]: quality or bitrate, speed preset, keyframe interval, audio bitrate and
//!   encoder threads, with [`EncodeSettings::check`] and [`check_picture`] refusing hostile values
//!   with the error codes of [`EncodeError::code`].
//! - [`rgba_to_i420`]: 8-bit sRGB RGBA to BT.709 limited-range 4:2:0, the layout the encoder takes.
//! - [`y4m`] and [`wav`]: readers for the command's inputs (YUV4MPEG2 4:2:0 8-bit, PCM/float WAV).
//! - With `media-encode`: `av1::Av1Encoder`, `opus::OpusStreamEncoder` and `webm::WebmEncoder`,
//!   the whole pipeline (video packets spooled to disk while the frames arrive, the sound encoded and
//!   both interleaved into the final file at `webm::WebmEncoder::finish`).

use serde::{Deserialize, Serialize};

#[cfg(any(test, feature = "media-encode"))]
use std::ffi::{OsStr, OsString};
#[cfg(any(test, feature = "media-encode"))]
use std::path::{Path, PathBuf};
#[cfg(any(test, feature = "media-encode"))]
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(feature = "media-encode")]
pub mod av1;
#[cfg(feature = "media-encode")]
#[allow(unsafe_code)] // FFI-shaped encoder API (docs/04 § 5, ADR-1593, ADR-3369)
pub mod opus;
pub mod wav;
#[cfg(feature = "media-encode")]
pub mod webm;
pub mod y4m;

/// The error code of a build without the encoder (`media-encode` off).
pub const UNAVAILABLE: &str = "media_encode_unavailable";

/// Smallest picture side the AV1 encoder takes.
pub const MIN_SIDE: u32 = 16;
/// Largest picture side (the render-out limit).
pub const MAX_SIDE: u32 = 8192;
/// Most pixels one picture may have (AV1 level 6.x: 8192 x 4352).
pub const MAX_PIXELS: u64 = 8192 * 4352;
/// Longest keyframe interval, in frames. Seeking decodes from the keyframe before the target, so a
/// longer interval makes every seek slower; 600 is 10 s at 60 fps, 25 s at 24 fps.
pub const MAX_KEYFRAME_INTERVAL: u32 = 600;
/// Most encoder threads a request may ask for.
pub const MAX_THREADS: u32 = 64;
/// Highest frame rate the encoder takes, frames per second.
pub const MAX_FPS: u32 = 240;
/// Video bitrate range, kbit/s.
pub const VIDEO_KBPS: std::ops::RangeInclusive<u32> = 50..=200_000;
/// Opus bitrate range, kbit/s (the codec's own limits are 6..=510).
pub const AUDIO_KBPS: std::ops::RangeInclusive<u32> = 16..=510;
/// Sample rates the Opus encoder takes without resampling.
pub const AUDIO_RATES: [u32; 5] = [8_000, 12_000, 16_000, 24_000, 48_000];

/// Why an encode was refused or failed. `code` is the protocol error code (lowercase ascii and `_`,
/// `book/agents/video-encode.md`); `message` says what to change.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {message}")]
pub struct EncodeError {
    /// Protocol code: [`UNAVAILABLE`], `encode_bad_size`, `encode_odd_size`, `encode_too_large`,
    /// `encode_bad_rate`, `encode_bad_quality`, `encode_bad_bitrate`, `encode_bad_speed`,
    /// `encode_bad_keyframe_interval`, `encode_bad_threads`, `encode_audio_format`,
    /// `encode_bad_input`, `encode_io` or `encode_failed`.
    pub code: &'static str,
    /// What went wrong, for a person.
    pub message: String,
}

impl EncodeError {
    /// An error with `code`.
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// The error of a build without the encoder.
    pub fn unavailable() -> Self {
        Self::new(
            UNAVAILABLE,
            "this build has no video encoder: build with the `media-encode` feature (regia, regia_dylib, \
             regia_agent or regia_media)",
        )
    }

    /// A file error on `what`.
    pub fn io(what: &std::path::Path, err: &std::io::Error) -> Self {
        Self::new("encode_io", format!("{}: {err}", what.display()))
    }

    /// The protocol code.
    pub fn code(&self) -> &'static str {
        self.code
    }
}

/// True when this build links the encoders (feature `media-encode`).
pub const fn available() -> bool {
    cfg!(feature = "media-encode")
}

/// Encoder settings, shared by `render.out` (`encode`), the render-job file (`output.encode`) and the
/// `regia media encode` command. Every field has a default, so `()` (RON) or `{}`
/// (JSON) is the default encode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EncodeSettings {
    /// Constant quality: the AV1 quantizer, 1..=255, lower is better and larger (default 80). Used
    /// when `bitrate_kbps` is 0.
    pub quality: u8,
    /// Target video bitrate in kbit/s (50..=200000); 0 (the default) is constant quality.
    pub bitrate_kbps: u32,
    /// Speed preset 0..=10: 0 is the slowest and smallest, 10 the fastest (default 6).
    pub speed: u8,
    /// Longest run of frames between keyframes, 1..=600 (default 240). The encoder may place a
    /// keyframe earlier on a scene cut.
    pub keyframe_interval: u32,
    /// Opus bitrate in kbit/s, 16..=510 (default 128).
    pub audio_kbps: u32,
    /// Encoder worker threads, 0..=64. 0 (the default) picks half the cores (1..=8); 1 is the
    /// single-threaded mode whose output is byte-identical run to run.
    pub threads: u32,
}

impl Default for EncodeSettings {
    fn default() -> Self {
        Self {
            quality: 80,
            bitrate_kbps: 0,
            speed: 6,
            keyframe_interval: 240,
            audio_kbps: 128,
            threads: 0,
        }
    }
}

impl EncodeSettings {
    /// Refuses out-of-range settings.
    ///
    /// # Errors
    /// `encode_bad_quality`, `encode_bad_bitrate`, `encode_bad_speed`, `encode_bad_keyframe_interval`,
    /// `encode_bad_threads` or `encode_audio_format`.
    pub fn check(&self) -> Result<(), EncodeError> {
        if self.quality == 0 {
            return Err(EncodeError::new(
                "encode_bad_quality",
                "`quality` must be 1..=255 (the AV1 quantizer; lower is better)",
            ));
        }
        if self.bitrate_kbps != 0 && !VIDEO_KBPS.contains(&self.bitrate_kbps) {
            return Err(EncodeError::new(
                "encode_bad_bitrate",
                format!(
                    "`bitrate_kbps` must be 0 (constant quality) or {}..={}",
                    VIDEO_KBPS.start(),
                    VIDEO_KBPS.end()
                ),
            ));
        }
        if self.speed > 10 {
            return Err(EncodeError::new("encode_bad_speed", "`speed` must be 0..=10"));
        }
        if !(1..=MAX_KEYFRAME_INTERVAL).contains(&self.keyframe_interval) {
            return Err(EncodeError::new(
                "encode_bad_keyframe_interval",
                format!("`keyframe_interval` must be 1..={MAX_KEYFRAME_INTERVAL} frames"),
            ));
        }
        if !AUDIO_KBPS.contains(&self.audio_kbps) {
            return Err(EncodeError::new(
                "encode_audio_format",
                format!(
                    "`audio_kbps` must be {}..={}",
                    AUDIO_KBPS.start(),
                    AUDIO_KBPS.end()
                ),
            ));
        }
        if self.threads > MAX_THREADS {
            return Err(EncodeError::new(
                "encode_bad_threads",
                format!("`threads` must be 0..={MAX_THREADS}"),
            ));
        }
        Ok(())
    }

    /// The encoder threads this request uses: `threads`, or half of `cores` (1..=8) for 0.
    pub fn worker_threads(&self, cores: usize) -> usize {
        if self.threads == 0 {
            (cores / 2).clamp(1, 8)
        } else {
            self.threads as usize
        }
    }

    /// Parses the JSON form (`render.out` `encode`): an object whose keys
    /// are the field names; a missing key keeps its default, an unknown key or a wrong type is refused.
    ///
    /// # Errors
    /// `encode_bad_input` naming the key, or a [`Self::check`] error.
    pub fn from_json(value: &serde_json::Value) -> Result<Self, EncodeError> {
        const KEYS: [&str; 6] = [
            "quality",
            "bitrate_kbps",
            "speed",
            "keyframe_interval",
            "audio_kbps",
            "threads",
        ];
        let object = match value {
            serde_json::Value::Null => return Ok(Self::default()),
            serde_json::Value::Object(object) => object,
            _ => return Err(EncodeError::new("encode_bad_input", "`encode` must be an object")),
        };
        if let Some(key) = object.keys().find(|k| !KEYS.contains(&k.as_str())) {
            return Err(EncodeError::new(
                "encode_bad_input",
                format!("unknown encode setting `{key}` (known: {})", KEYS.join(", ")),
            ));
        }
        let settings: Self = serde_json::from_value(value.clone())
            .map_err(|e| EncodeError::new("encode_bad_input", format!("`encode`: {e}")))?;
        settings.check()?;
        Ok(settings)
    }
}

/// Refuses a picture size or frame rate the encoder cannot take: sides below [`MIN_SIDE`] or zero,
/// odd sides (4:2:0 halves both), a picture over [`MAX_SIDE`] or [`MAX_PIXELS`], a zero or
/// above-[`MAX_FPS`] frame rate.
///
/// # Errors
/// `encode_bad_size`, `encode_odd_size`, `encode_too_large` or `encode_bad_rate`.
pub fn check_picture(width: u32, height: u32, fps_num: u32, fps_den: u32) -> Result<(), EncodeError> {
    if width < MIN_SIDE || height < MIN_SIDE {
        return Err(EncodeError::new(
            "encode_bad_size",
            format!("{width}x{height}: both sides must be at least {MIN_SIDE} pixels"),
        ));
    }
    if !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        return Err(EncodeError::new(
            "encode_odd_size",
            format!("{width}x{height}: 4:2:0 video needs an even width and height"),
        ));
    }
    if width > MAX_SIDE || height > MAX_SIDE || u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(EncodeError::new(
            "encode_too_large",
            format!("{width}x{height}: at most {MAX_SIDE} pixels a side and {MAX_PIXELS} pixels"),
        ));
    }
    if fps_num == 0 || fps_den == 0 || u64::from(fps_num) > u64::from(MAX_FPS) * u64::from(fps_den) {
        return Err(EncodeError::new(
            "encode_bad_rate",
            format!("frame rate {fps_num}/{fps_den}: must be above 0 and at most {MAX_FPS} fps"),
        ));
    }
    Ok(())
}

/// Bytes of one 8-bit 4:2:0 picture of `width` x `height` (even sides).
pub fn i420_bytes(width: u32, height: u32) -> usize {
    let (w, h) = (width as usize, height as usize);
    w * h + 2 * (w / 2) * (h / 2)
}

/// Presentation time of frame `k` at `fps_num / fps_den`, in nanoseconds (exact, rounded down).
pub fn frame_time_ns(k: u64, fps_num: u32, fps_den: u32) -> i64 {
    let ns = u128::from(k) * u128::from(fps_den) * 1_000_000_000 / u128::from(fps_num.max(1));
    i64::try_from(ns).unwrap_or(i64::MAX)
}

/// Converts 8-bit sRGB-encoded RGBA (alpha ignored, rows tightly packed) to BT.709 limited-range
/// 4:2:0: Y from each pixel, U and V from the average of each 2 x 2 block's R'G'B' (center-sited).
/// `width` and `height` must be even; returns `None` when `rgba` is not `width * height * 4` bytes.
///
/// The matrix is ITU-R BT.709-6 § 3 (Kr 0.2126, Kb 0.0722) with 8-bit studio range (Y 16..=235,
/// C 16..=240), in 16.16 fixed point so every host gives the same bytes.
pub fn rgba_to_i420(rgba: &[u8], width: u32, height: u32) -> Option<Vec<u8>> {
    let (w, h) = (width as usize, height as usize);
    if rgba.len() != w.checked_mul(h)?.checked_mul(4)? || !w.is_multiple_of(2) || !h.is_multiple_of(2) {
        return None;
    }
    // Y = 16 + 219 (Kr R + Kg G + Kb B) / 255; Cb, Cr = 128 + 224 (B - Y') / (2 (1 - Kb)) / 255 ...
    // Coefficients scaled by 65536 (rounded): see docs/provenance.md (BT.709 RGB to YCbCr).
    const YR: i64 = 11_966; // 0.2126 * 219 / 255 * 65536
    const YG: i64 = 40_254; // 0.7152 * 219 / 255 * 65536
    const YB: i64 = 4_064; // 0.0722 * 219 / 255 * 65536
    const UR: i64 = -6_596; // -0.2126 / 1.8556 * 224 / 255 * 65536
    const UG: i64 = -22_189; // -0.7152 / 1.8556 * 224 / 255 * 65536
    const UB: i64 = 28_784; // 0.5 * 224 / 255 * 65536
    const VR: i64 = 28_784; // 0.5 * 224 / 255 * 65536
    const VG: i64 = -26_145; // -0.7152 / 1.5748 * 224 / 255 * 65536
    const VB: i64 = -2_639; // -0.0722 / 1.5748 * 224 / 255 * 65536
    const HALF: i64 = 1 << 15;
    let mut out = vec![0u8; i420_bytes(width, height)];
    let (luma, chroma) = out.split_at_mut(w * h);
    let (cb, cr) = chroma.split_at_mut((w / 2) * (h / 2));
    let px = |x: usize, y: usize| {
        let i = (y * w + x) * 4;
        (i64::from(rgba[i]), i64::from(rgba[i + 1]), i64::from(rgba[i + 2]))
    };
    for y in 0..h {
        for x in 0..w {
            let (r, g, b) = px(x, y);
            luma[y * w + x] = (16 + ((YR * r + YG * g + YB * b + HALF) >> 16)).clamp(16, 235) as u8;
        }
    }
    for cy in 0..h / 2 {
        for cx in 0..w / 2 {
            let (mut r, mut g, mut b) = (0, 0, 0);
            for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                let (pr, pg, pb) = px(cx * 2 + dx, cy * 2 + dy);
                r += pr;
                g += pg;
                b += pb;
            }
            // The sums are 4x the average: fold the /4 into the shift.
            let u = 128 + ((UR * r + UG * g + UB * b + (HALF << 2)) >> 18);
            let v = 128 + ((VR * r + VG * g + VB * b + (HALF << 2)) >> 18);
            cb[cy * (w / 2) + cx] = u.clamp(16, 240) as u8;
            cr[cy * (w / 2) + cx] = v.clamp(16, 240) as u8;
        }
    }
    Some(out)
}

/// Peak signal-to-noise ratio in dB of two equal-length 8-bit sample runs (`f64::INFINITY` when they
/// are equal; `None` when the lengths differ or are zero). Used by the tests and the encode report.
pub fn psnr(a: &[u8], b: &[u8]) -> Option<f64> {
    if a.len() != b.len() || a.is_empty() {
        return None;
    }
    let sse: u64 = a
        .iter()
        .zip(b)
        .map(|(x, y)| {
            let d = i64::from(*x) - i64::from(*y);
            (d * d) as u64
        })
        .sum();
    if sse == 0 {
        return Some(f64::INFINITY);
    }
    let mse = sse as f64 / a.len() as f64;
    Some(10.0 * (255.0 * 255.0 / mse).log10())
}

/// What an encode produced.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EncodeReport {
    /// The file written.
    pub path: String,
    /// Video frames in the file.
    pub frames: u64,
    /// Keyframes among them.
    pub keyframes: u64,
    /// Declared duration, nanoseconds.
    pub duration_ns: u64,
    /// Bytes of the file.
    pub bytes: u64,
    /// Opus packets, 0 without sound.
    pub audio_packets: u64,
    /// Sound samples per channel encoded (before the Opus pre-skip), 0 without sound.
    pub audio_samples: u64,
    /// Clusters written.
    pub clusters: u64,
    /// Cue points written.
    pub cues: u64,
}

/// A same-directory name for moving `dest` aside. Unique across processes and threads so two
/// encodes finishing together do not share a backup.
#[cfg(any(test, feature = "media-encode"))]
fn replace_backup(dest: &Path) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let mut name = OsString::from(".");
    name.push(dest.file_name().unwrap_or(OsStr::new("output")));
    name.push(".");
    name.push(std::process::id().to_string());
    name.push(".");
    name.push(n.to_string());
    name.push(".replacing");
    dest.with_file_name(name)
}

/// Renames the finished part file over the destination. On Windows a rename over a file another
/// process holds open is refused, while Linux and macOS replace silently. When the first rename
/// fails and `dest` is a regular file (an earlier take), it is moved aside and restored if the
/// part still cannot take its place. The previous output is never deleted first: the caller's
/// `Drop` still removes the part file. Anything else at `dest` (a directory, a link) is not an
/// earlier take: it stays where it is and the first error is returned.
#[cfg(any(test, feature = "media-encode"))]
pub(crate) fn replace_finished_part(part: &Path, dest: &Path) -> std::io::Result<()> {
    match std::fs::rename(part, dest) {
        Ok(()) => Ok(()),
        Err(first) => {
            // A directory moved aside stayed hidden under the backup name (`remove_file` does
            // not remove it) while the part took its name and the encode reported success.
            let earlier_take = std::fs::symlink_metadata(dest).is_ok_and(|meta| meta.file_type().is_file());
            if !earlier_take {
                return Err(first);
            }
            let backup = replace_backup(dest);
            if std::fs::rename(dest, &backup).is_err() {
                return Err(first);
            }
            match std::fs::rename(part, dest) {
                Ok(()) => {
                    let _ = std::fs::remove_file(&backup);
                    Ok(())
                }
                Err(_) => {
                    let _ = std::fs::rename(&backup, dest);
                    Err(first)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_pass_and_hostile_settings_are_refused_with_their_codes() {
        assert!(EncodeSettings::default().check().is_ok());
        let code = |s: EncodeSettings| s.check().err().map(|e| e.code);
        let d = EncodeSettings::default;
        assert_eq!(
            code(EncodeSettings { quality: 0, ..d() }),
            Some("encode_bad_quality")
        );
        assert_eq!(
            code(EncodeSettings {
                bitrate_kbps: 10,
                ..d()
            }),
            Some("encode_bad_bitrate")
        );
        assert_eq!(
            code(EncodeSettings { speed: 11, ..d() }),
            Some("encode_bad_speed")
        );
        assert_eq!(
            code(EncodeSettings {
                keyframe_interval: 0,
                ..d()
            }),
            Some("encode_bad_keyframe_interval")
        );
        assert_eq!(
            code(EncodeSettings {
                keyframe_interval: u32::MAX,
                ..d()
            }),
            Some("encode_bad_keyframe_interval")
        );
        assert_eq!(
            code(EncodeSettings { audio_kbps: 5, ..d() }),
            Some("encode_audio_format")
        );
        assert_eq!(
            code(EncodeSettings { threads: 1000, ..d() }),
            Some("encode_bad_threads")
        );
    }

    #[test]
    fn picture_checks_refuse_zero_odd_huge_and_bad_rates() {
        let code = |w, h, n, d| check_picture(w, h, n, d).err().map(|e| e.code);
        assert_eq!(code(128, 72, 24, 1), None);
        assert_eq!(code(0, 72, 24, 1), Some("encode_bad_size"));
        assert_eq!(code(128, 0, 24, 1), Some("encode_bad_size"));
        assert_eq!(code(129, 72, 24, 1), Some("encode_odd_size"));
        assert_eq!(code(128, 73, 24, 1), Some("encode_odd_size"));
        assert_eq!(code(16_384, 16, 24, 1), Some("encode_too_large"));
        assert_eq!(code(8192, 8192, 24, 1), Some("encode_too_large"));
        assert_eq!(code(128, 72, 0, 1), Some("encode_bad_rate"));
        assert_eq!(code(128, 72, 24, 0), Some("encode_bad_rate"));
        assert_eq!(code(128, 72, 1000, 1), Some("encode_bad_rate"));
        assert_eq!(code(128, 72, 30_000, 1001), None);
    }

    #[test]
    fn json_settings_keep_defaults_and_refuse_unknown_keys() {
        let s = EncodeSettings::from_json(&serde_json::json!({ "quality": 40, "threads": 1 })).expect("ok");
        assert_eq!((s.quality, s.threads, s.speed), (40, 1, 6));
        assert_eq!(
            EncodeSettings::from_json(&serde_json::json!({ "qualty": 40 }))
                .err()
                .map(|e| e.code),
            Some("encode_bad_input")
        );
        assert_eq!(
            EncodeSettings::from_json(&serde_json::json!({ "quality": "high" }))
                .err()
                .map(|e| e.code),
            Some("encode_bad_input")
        );
        assert_eq!(
            EncodeSettings::from_json(&serde_json::json!({ "keyframe_interval": 100000 }))
                .err()
                .map(|e| e.code),
            Some("encode_bad_keyframe_interval")
        );
        assert_eq!(
            EncodeSettings::from_json(&serde_json::Value::Null).ok(),
            Some(EncodeSettings::default())
        );
    }

    #[test]
    fn rgba_conversion_hits_the_bt709_studio_levels() {
        // White, black, and the primaries, each a whole 2x2 block.
        let block = |rgb: [u8; 3]| {
            let rgba: Vec<u8> = (0..4).flat_map(|_| [rgb[0], rgb[1], rgb[2], 255]).collect();
            let yuv = rgba_to_i420(&rgba, 2, 2).expect("2x2");
            (yuv[0], yuv[4], yuv[5])
        };
        assert_eq!(block([255, 255, 255]), (235, 128, 128));
        assert_eq!(block([0, 0, 0]), (16, 128, 128));
        // BT.709 red: Y 63, Cb 102, Cr 240.
        assert_eq!(block([255, 0, 0]), (63, 102, 240));
        // Green: Y 173, Cb 42, Cr 26. Blue: Y 32, Cb 240, Cr 118.
        assert_eq!(block([0, 255, 0]), (173, 42, 26));
        assert_eq!(block([0, 0, 255]), (32, 240, 118));
        assert!(rgba_to_i420(&[0; 12], 2, 2).is_none());
        assert!(rgba_to_i420(&[0; 12], 3, 1).is_none());
    }

    #[test]
    fn frame_times_are_exact_rationals() {
        assert_eq!(frame_time_ns(0, 24, 1), 0);
        assert_eq!(frame_time_ns(24, 24, 1), 1_000_000_000);
        assert_eq!(frame_time_ns(1, 30_000, 1001), 33_366_666);
        assert_eq!(i420_bytes(128, 72), 128 * 72 * 3 / 2);
    }

    #[test]
    fn psnr_of_equal_and_offset_runs() {
        assert_eq!(psnr(&[1, 2, 3], &[1, 2, 3]), Some(f64::INFINITY));
        let p = psnr(&[10; 100], &[11; 100]).expect("same length");
        assert!((p - 48.13).abs() < 0.01, "{p}");
        assert!(psnr(&[1], &[1, 2]).is_none());
    }

    #[test]
    fn a_failed_replace_keeps_the_previous_output() {
        let dir = regia_test_utils::unique_temp_dir("webm-replace");
        let dest = dir.join("out.webm");
        let part = dir.join("missing.webm.part");
        std::fs::write(&dest, b"old-encode").expect("dest");
        let err = replace_finished_part(&part, &dest).expect_err("missing part");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        assert_eq!(
            std::fs::read(&dest).expect("dest still there"),
            b"old-encode",
            "a failed replace removed the previous encode"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A directory at the output path is not an earlier take: the replace fails and the directory
    /// stays where it is. Moved aside like a held-open file, it stayed hidden under the backup name
    /// (`remove_file` does not remove a directory), the part took its name, and the encode
    /// reported success. integ-v048b's version only retried for a Windows access error; the
    /// move-aside that the integ-v048 resolution kept ran for any error.
    #[test]
    fn a_directory_at_the_output_path_stays_and_the_replace_fails() {
        let dir = regia_test_utils::unique_temp_dir("webm-replace-dir");
        let dest = dir.join("captures");
        std::fs::create_dir_all(&dest).expect("dest dir");
        std::fs::write(dest.join("keep.txt"), b"keep").expect("file inside");
        let part = dir.join("captures.part");
        std::fs::write(&part, b"new-encode").expect("part");
        let result = replace_finished_part(&part, &dest);
        assert!(result.is_err(), "the encode took the place of a directory");
        assert!(dest.is_dir(), "the directory left the output path");
        assert_eq!(std::fs::read(dest.join("keep.txt")).expect("kept"), b"keep");
        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .expect("read dir")
            .map(|entry| entry.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["captures", "captures.part"], "nothing was moved aside");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_successful_replace_writes_the_new_output() {
        let dir = regia_test_utils::unique_temp_dir("webm-replace-ok");
        let dest = dir.join("out.webm");
        let part = dir.join("out.webm.part");
        std::fs::write(&dest, b"old-encode").expect("dest");
        std::fs::write(&part, b"new-encode").expect("part");
        replace_finished_part(&part, &dest).expect("replace");
        assert_eq!(std::fs::read(&dest).expect("dest"), b"new-encode");
        assert!(
            !part.exists(),
            "the part file is still beside the finished output"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
