// SPDX-License-Identifier: MPL-2.0
//! The decoder isolation seam (ADR-1593 § 4, ADR-1670): every decoder call goes through
//! [`GuardedVideo`] / [`GuardedAudio`], which enforce the hard caps before the call (packet size,
//! picture size, frame-queue bytes), count corrupt packets, fail the session after too many in a row,
//! and time each call against a per-packet budget. Rust decoders additionally run under
//! `catch_unwind` (a caught panic is a bug and fails the session); native decoders cannot be caught
//! and rely on the caps alone. The catch works in unwinding builds only: the shipping profile is
//! `panic = "abort"`, where a decoder panic ends the game with a crash report (audit v034 net L6).
//! Decoders see packets only: they have no access to files or the VFS.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::codec::{AudioDecoder, DecodedPicture, VideoDecoder};
use crate::container::Packet;
use crate::error::MediaError;

/// Hard caps checked before every decoder call.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DecodeCaps {
    /// Largest picture width.
    pub max_width: u32,
    /// Largest picture height.
    pub max_height: u32,
    /// Largest packet handed to a decoder.
    pub max_packet_bytes: usize,
    /// Largest total bytes of decoded pictures one call may return.
    pub max_output_bytes: usize,
    /// Per-packet time budget; calls over it are counted, not interrupted.
    pub packet_budget: Duration,
    /// Consecutive rejected packets before the session fails.
    pub max_consecutive_corrupt: u32,
    /// Media time of consecutive rejected packets before the session fails.
    pub max_corrupt_media_ns: u64,
    /// Media time of packets dropped while waiting for a keyframe after a rejected one before the
    /// session fails: longer than any real keyframe interval, so a stream that has no more
    /// keyframes fails instead of freezing with no event.
    pub max_keyframe_wait_ns: u64,
}

impl DecodeCaps {
    /// Desktop defaults (specs/50 § 2.4): 3840x2160.
    pub const DESKTOP: Self = Self {
        max_width: 3840,
        max_height: 2160,
        max_packet_bytes: 16 * 1024 * 1024,
        max_output_bytes: 8 * 3840 * 2160 * 3,
        packet_budget: Duration::from_millis(100),
        max_consecutive_corrupt: 30,
        max_corrupt_media_ns: 2_000_000_000,
        max_keyframe_wait_ns: 30_000_000_000,
    };

    /// Android, and a desktop build with `SteamDeck=1`: 1920x1080.
    pub const HANDHELD: Self = Self {
        max_width: 1920,
        max_height: 1080,
        max_output_bytes: 8 * 1920 * 1080 * 3,
        ..Self::DESKTOP
    };

    /// Picture caps for an Android build and for a host that set `SteamDeck=1`.
    /// Tests pass the flags; they do not read the process environment.
    pub fn for_host(android: bool, steam_deck: bool) -> Self {
        if android || steam_deck {
            Self::HANDHELD
        } else {
            Self::DESKTOP
        }
    }

    /// The defaults for this process: Android, or `SteamDeck=1`, uses [`Self::HANDHELD`].
    pub fn for_platform() -> Self {
        let steam_deck = regia_core::brand::env_var("SteamDeck").ok().as_deref() == Some("1");
        Self::for_host(cfg!(target_os = "android"), steam_deck)
    }

    /// AV1 worker threads for the process (specs/50 § 2.4): 2 on Android, otherwise
    /// `(cores / 4)` clamped to `1..=4`. A zero result is not allowed; every decoder still gets a
    /// thread of its own when the pool is spent.
    pub fn av1_process_budget(android: bool, cores: u32) -> u32 {
        if android { 2 } else { (cores / 4).clamp(1, 4) }
    }

    /// Refuses a picture size over the caps (`limit_exceeded`).
    pub fn check_size(&self, w: u32, h: u32) -> Result<(), MediaError> {
        if w == 0 || h == 0 || w > self.max_width || h > self.max_height {
            return Err(MediaError::LimitExceeded(format!(
                "picture {w}x{h} is outside 1x1..={}x{}",
                self.max_width, self.max_height
            )));
        }
        Ok(())
    }
}

impl Default for DecodeCaps {
    fn default() -> Self {
        Self::for_platform()
    }
}

/// Counters of the guard.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct GuardStats {
    /// Packets handed to the decoder.
    pub packets: u64,
    /// Packets rejected (by a cap or the decoder).
    pub corrupt: u64,
    /// Calls over the per-packet budget.
    pub over_budget: u64,
    /// Exponential moving average of decode time per packet, in milliseconds.
    pub decode_ms_ema: f64,
    /// Longest single call, in milliseconds.
    pub decode_ms_max: f64,
    /// Pictures took longer to decode than their frame period for [`SLOW_DECODE_FRAMES`] in a row.
    pub decode_too_slow: bool,
}

/// What one guarded call produced.
#[derive(Debug)]
pub enum Guarded<T> {
    /// Decoded output.
    Ok(T),
    /// The packet was rejected and dropped; decoding resumes at the next keyframe.
    Dropped,
}

#[derive(Debug, Default)]
struct Streak {
    count: u32,
    first_ns: Option<i64>,
}

impl Streak {
    fn bad(&mut self, pts: i64, caps: &DecodeCaps) -> Result<(), MediaError> {
        self.count += 1;
        let first = *self.first_ns.get_or_insert(pts);
        let span = pts.saturating_sub(first).max(0) as u64;
        if self.count >= caps.max_consecutive_corrupt || span >= caps.max_corrupt_media_ns {
            return Err(MediaError::DecodeFailed(format!(
                "{} consecutive corrupt packets over {} ms of media",
                self.count,
                span / 1_000_000
            )));
        }
        Ok(())
    }

    fn good(&mut self) {
        self.count = 0;
        self.first_ns = None;
    }
}

/// Consecutive pictures over their frame period before a clip counts as too heavy (specs/50 § 10).
pub const SLOW_DECODE_FRAMES: u32 = 60;

/// The specs/50 § 10 check: decode time per shown picture (hidden frames included) against the time
/// to the previous picture. Presentation times give the period, so clips without a declared frame
/// rate are covered too.
#[derive(Debug, Default)]
struct SlowDecode {
    spent_ms: f64,
    last_pts: Option<i64>,
    run: u32,
}

impl SlowDecode {
    /// Adds one call's time; `shown_pts` is the picture it produced, if any. True once the run of
    /// slow pictures reaches [`SLOW_DECODE_FRAMES`].
    fn observe(&mut self, ms: f64, shown_pts: Option<i64>) -> bool {
        self.spent_ms += ms;
        let Some(pts) = shown_pts else {
            return false;
        };
        let spent = std::mem::take(&mut self.spent_ms);
        let Some(last) = self.last_pts.replace(pts) else {
            return false;
        };
        let period_ms = pts.saturating_sub(last) as f64 / 1e6;
        if period_ms > 0.0 && spent > period_ms {
            self.run += 1;
        } else {
            self.run = 0;
        }
        self.run >= SLOW_DECODE_FRAMES
    }
}

fn time_call<T>(stats: &mut GuardStats, budget: Duration, f: impl FnOnce() -> T) -> (T, f64) {
    let start = Instant::now();
    let out = f();
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    stats.decode_ms_ema = if stats.packets <= 1 {
        ms
    } else {
        stats.decode_ms_ema * 0.9 + ms * 0.1
    };
    stats.decode_ms_max = stats.decode_ms_max.max(ms);
    if ms > budget.as_secs_f64() * 1000.0 {
        stats.over_budget += 1;
    }
    (out, ms)
}

/// A video decoder behind the caps.
pub struct GuardedVideo {
    inner: Box<dyn VideoDecoder>,
    caps: DecodeCaps,
    stats: GuardStats,
    streak: Streak,
    need_key: bool,
    /// The first packet dropped while waiting for a keyframe.
    key_wait_from: Option<i64>,
    last_warn_ns: Option<i64>,
    slow: SlowDecode,
}

impl std::fmt::Debug for GuardedVideo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GuardedVideo")
            .field("decoder", &self.inner.name())
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

impl GuardedVideo {
    /// Wraps a decoder.
    pub fn new(inner: Box<dyn VideoDecoder>, caps: DecodeCaps) -> Self {
        Self {
            inner,
            caps,
            stats: GuardStats::default(),
            streak: Streak::default(),
            need_key: false,
            key_wait_from: None,
            last_warn_ns: None,
            slow: SlowDecode::default(),
        }
    }

    /// Counters.
    pub fn stats(&self) -> GuardStats {
        self.stats
    }

    /// The decoder's name.
    pub fn name(&self) -> &'static str {
        self.inner.name()
    }

    /// Flushes the decoder (after a seek).
    pub fn flush(&mut self) {
        self.inner.flush();
        self.need_key = false;
        self.key_wait_from = None;
        self.streak.good();
        self.slow = SlowDecode::default();
    }

    fn reject(&mut self, pts: i64, why: &str) -> Result<Guarded<Vec<DecodedPicture>>, MediaError> {
        self.stats.corrupt += 1;
        self.need_key = true;
        if self
            .last_warn_ns
            .is_none_or(|t| pts.saturating_sub(t) >= 1_000_000_000)
        {
            tracing::warn!(pts_ns = pts, "video packet dropped: {why}");
            self.last_warn_ns = Some(pts);
        }
        self.streak.bad(pts, &self.caps)?;
        Ok(Guarded::Dropped)
    }

    /// Decodes one packet under the caps. `Err` means the session must fail (`decode_failed`).
    pub fn decode(&mut self, packet: &Packet) -> Result<Guarded<Vec<DecodedPicture>>, MediaError> {
        if self.need_key && !packet.keyframe {
            let from = *self.key_wait_from.get_or_insert(packet.pts_ns);
            let waited = packet.pts_ns.saturating_sub(from).max(0) as u64;
            if waited >= self.caps.max_keyframe_wait_ns {
                return Err(MediaError::DecodeFailed(format!(
                    "no keyframe in {} ms after a corrupt packet",
                    waited / 1_000_000
                )));
            }
            return Ok(Guarded::Dropped);
        }
        self.key_wait_from = None;
        if packet.data.len() > self.caps.max_packet_bytes {
            return self.reject(packet.pts_ns, "packet over the size cap");
        }
        self.stats.packets += 1;
        let budget = self.caps.packet_budget;
        let inner = &mut self.inner;
        let (result, ms) = time_call(&mut self.stats, budget, || {
            catch_unwind(AssertUnwindSafe(|| inner.decode(packet)))
        });
        match result {
            Err(_) => Err(MediaError::DecodeFailed(format!(
                "the {} decoder panicked (a bug)",
                self.inner.name()
            ))),
            Ok(Err(e)) => self.reject(packet.pts_ns, &e.to_string()),
            Ok(Ok(pictures)) => {
                let bytes: usize = pictures.iter().map(DecodedPicture::byte_size).sum();
                if bytes > self.caps.max_output_bytes {
                    return self.reject(packet.pts_ns, "decoded output over the frame-queue cap");
                }
                if let Some(p) = pictures
                    .iter()
                    .find(|p| self.caps.check_size(p.width, p.height).is_err())
                {
                    let why = format!("picture {}x{} over the size cap", p.width, p.height);
                    return self.reject(packet.pts_ns, &why);
                }
                self.need_key = false;
                self.streak.good();
                if self.slow.observe(ms, pictures.last().map(|p| p.pts_ns)) {
                    self.stats.decode_too_slow = true;
                }
                Ok(Guarded::Ok(pictures))
            }
        }
    }

    /// Pictures the decoder still holds at the end of the stream, under the same output caps as
    /// [`Self::decode`]. A failing or panicking drain loses those pictures and is logged, never fatal.
    pub fn drain(&mut self) -> Vec<DecodedPicture> {
        let inner = &mut self.inner;
        match catch_unwind(AssertUnwindSafe(|| inner.drain())) {
            Ok(Ok(pictures)) => {
                let bytes: usize = pictures.iter().map(DecodedPicture::byte_size).sum();
                let sizes_ok = pictures
                    .iter()
                    .all(|p| self.caps.check_size(p.width, p.height).is_ok());
                if bytes > self.caps.max_output_bytes || !sizes_ok {
                    tracing::warn!("end-of-stream pictures over the caps dropped");
                    return Vec::new();
                }
                pictures
            }
            Ok(Err(e)) => {
                tracing::warn!("end-of-stream drain failed: {e}");
                Vec::new()
            }
            Err(_) => {
                tracing::warn!(
                    "the {} decoder panicked while draining (a bug)",
                    self.inner.name()
                );
                Vec::new()
            }
        }
    }
}

/// An audio decoder behind the caps. A rejected packet becomes silence of the packet's nominal length
/// so the audio clock keeps advancing (specs/50 § 2.9).
pub struct GuardedAudio {
    inner: Box<dyn AudioDecoder>,
    caps: DecodeCaps,
    stats: GuardStats,
}

impl std::fmt::Debug for GuardedAudio {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GuardedAudio")
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

impl GuardedAudio {
    /// Wraps a decoder.
    pub fn new(inner: Box<dyn AudioDecoder>, caps: DecodeCaps) -> Self {
        Self {
            inner,
            caps,
            stats: GuardStats::default(),
        }
    }

    /// Counters.
    pub fn stats(&self) -> GuardStats {
        self.stats
    }

    /// Flushes the decoder.
    pub fn flush(&mut self) {
        self.inner.flush();
    }

    /// The decoder's output channels and rate when they differ from the track header.
    pub fn output_format(&self) -> Option<(u8, u32)> {
        self.inner.output_format()
    }

    /// Decodes into `out`; on rejection, appends `silence_samples` zeros instead.
    pub fn decode(
        &mut self,
        packet: &Packet,
        out: &mut Vec<f32>,
        silence_samples: usize,
    ) -> Result<(), MediaError> {
        let start = out.len();
        if packet.data.len() > self.caps.max_packet_bytes {
            self.stats.corrupt += 1;
            out.resize(start + silence_samples, 0.0);
            return Ok(());
        }
        self.stats.packets += 1;
        let budget = self.caps.packet_budget;
        let inner = &mut self.inner;
        let (result, _) = time_call(&mut self.stats, budget, || {
            catch_unwind(AssertUnwindSafe(|| inner.decode(packet, out)))
        });
        match result {
            Err(_) => Err(MediaError::DecodeFailed(
                "the audio decoder panicked (a bug)".into(),
            )),
            Ok(Err(_)) => {
                self.stats.corrupt += 1;
                out.truncate(start);
                out.resize(start + silence_samples, 0.0);
                Ok(())
            }
            Ok(Ok(())) => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME_NS: i64 = 33_333_333;

    /// Feeds `n` pictures of `ms` each, returns whether the run was reached on the last one.
    fn feed(slow: &mut SlowDecode, first: i64, n: u32, ms: f64) -> bool {
        (0..n)
            .map(|i| slow.observe(ms, Some((first + i64::from(i)) * FRAME_NS)))
            .fold(false, |_, reached| reached)
    }

    #[test]
    fn slow_decode_needs_sixty_pictures_in_a_row() {
        let mut slow = SlowDecode::default();
        // The first picture has no period; 60 more over it reach the run, 59 do not.
        assert!(!feed(&mut slow, 0, 60, 40.0));
        assert!(slow.observe(40.0, Some(60 * FRAME_NS)));
    }

    #[test]
    fn one_fast_picture_restarts_the_run() {
        let mut slow = SlowDecode::default();
        assert!(!feed(&mut slow, 0, 50, 40.0));
        assert!(!slow.observe(1.0, Some(50 * FRAME_NS)));
        assert!(!feed(&mut slow, 51, 59, 40.0));
        assert!(slow.observe(40.0, Some(110 * FRAME_NS)));
    }

    #[test]
    fn hidden_frames_count_towards_the_next_shown_picture() {
        let mut slow = SlowDecode::default();
        slow.observe(0.0, Some(0));
        for i in 1..=SLOW_DECODE_FRAMES {
            // 20 ms alone is inside 33 ms; with its 20 ms hidden frame the picture is over it.
            assert!(!slow.observe(20.0, None));
            let reached = slow.observe(20.0, Some(i64::from(i) * FRAME_NS));
            assert_eq!(reached, i == SLOW_DECODE_FRAMES);
        }
    }

    #[test]
    fn equal_timestamps_never_count_as_slow() {
        let mut slow = SlowDecode::default();
        for _ in 0..200 {
            assert!(!slow.observe(500.0, Some(FRAME_NS)));
        }
    }

    #[test]
    fn host_caps_and_av1_budget_follow_the_flags() {
        assert_eq!(DecodeCaps::for_host(false, false), DecodeCaps::DESKTOP);
        assert_eq!(DecodeCaps::for_host(true, false).max_width, 1920);
        assert_eq!(DecodeCaps::for_host(false, true).max_height, 1080);
        assert_eq!(DecodeCaps::av1_process_budget(true, 32), 2);
        assert_eq!(DecodeCaps::av1_process_budget(false, 1), 1);
        assert_eq!(DecodeCaps::av1_process_budget(false, 8), 2);
        assert_eq!(DecodeCaps::av1_process_budget(false, 16), 4);
        assert_eq!(DecodeCaps::av1_process_budget(false, 64), 4);
    }
}
