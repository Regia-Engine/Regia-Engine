// SPDX-License-Identifier: MPL-2.0
//! The master output stage (ADR-544): a stereo-linked peak limiter on kira's main track, after
//! every bus and send bus, plus an output meter.
//!
//! kira hard-clips the final mix at ±1 and passes NaN through. A loud moment (many voices on
//! one bus, a reverb tail on top of an explosion) then flat-tops into harsh distortion, and a
//! single non-finite sample reaches the device. The limiter replaces both: non-finite samples
//! become silence (and are counted), and a gain with instant attack and an exponential release
//! keeps the peak of every frame at or below the ceiling. Below the ceiling the gain is exactly
//! 1, so a mix that never reaches it is rendered bit for bit as before.
//!
//! Algorithm: a feed-forward peak limiter (a compressor with an infinite ratio and a zero
//! attack time), as described by Giannoulis, Massberg and Reiss, "Digital Dynamic Range
//! Compressor Design — A Tutorial and Analysis", JAES 60(6), 2012, § 2–3: per frame the target
//! gain is `min(1, ceiling / peak)`; a lower target is taken at once, a higher one is approached
//! with the one-pole release `g ← t − (t − g)·e^(−dt/τ)`. The gain never rises above its target,
//! so `|out| ≤ ceiling` holds for every frame. No lookahead: the attack is a gain step on the
//! frame that crosses the ceiling, which is the same discontinuity a hard clip has, without the
//! clip's flat top.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use kira::{
    Frame,
    effect::{Effect, EffectBuilder},
    info::Info,
};

use crate::spatial::{db_to_gain, gain_to_db};

/// Settings of the master limiter (ADR-544). The default keeps every mix that stays below full
/// scale untouched and turns full-scale overs into gain reduction instead of clipping.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MasterLimiter {
    /// Output ceiling in dBFS (`MasterLimiter::MIN_CEILING_DB..=0`): no output sample exceeds
    /// it.
    pub ceiling_db: f32,
    /// Release time constant in seconds (`MasterLimiter::MIN_RELEASE_SECS..=MasterLimiter::MAX_RELEASE_SECS`):
    /// how fast the gain recovers after a peak.
    pub release_secs: f32,
}

impl MasterLimiter {
    /// The lowest ceiling accepted, dBFS.
    pub const MIN_CEILING_DB: f32 = -24.0;
    /// The shortest release accepted, seconds.
    pub const MIN_RELEASE_SECS: f32 = 0.001;
    /// The longest release accepted, seconds.
    pub const MAX_RELEASE_SECS: f32 = 2.0;

    /// The settings with every field clamped into its range (NaN takes the default).
    #[must_use]
    pub fn clamped(self) -> Self {
        let default = Self::default();
        let ceiling_db = if self.ceiling_db.is_nan() {
            default.ceiling_db
        } else {
            self.ceiling_db.clamp(Self::MIN_CEILING_DB, 0.0)
        };
        let release_secs = if self.release_secs.is_nan() {
            default.release_secs
        } else {
            self.release_secs
                .clamp(Self::MIN_RELEASE_SECS, Self::MAX_RELEASE_SECS)
        };
        Self {
            ceiling_db,
            release_secs,
        }
    }
}

impl Default for MasterLimiter {
    fn default() -> Self {
        Self {
            ceiling_db: 0.0,
            release_secs: 0.08,
        }
    }
}

/// What the output stage measured since the last [`crate::Audio::reset_output_meter`]: the
/// whole mix, after every bus and send bus.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct OutputMeter {
    /// Stereo frames processed.
    pub frames: u64,
    /// Highest sample magnitude entering the limiter (1 = full scale; above 1 would have
    /// clipped).
    pub peak_in: f32,
    /// Highest sample magnitude leaving the limiter.
    pub peak_out: f32,
    /// Deepest gain reduction applied, dB (0 = the limiter never acted).
    pub max_reduction_db: f32,
    /// Frames the limiter turned down.
    pub limited_frames: u64,
    /// NaN or infinite samples replaced by silence.
    pub nonfinite_samples: u64,
}

impl OutputMeter {
    /// [`OutputMeter::peak_in`] in dBFS (−100 for silence).
    pub fn peak_in_db(&self) -> f32 {
        gain_to_db(self.peak_in)
    }

    /// [`OutputMeter::peak_out`] in dBFS (−100 for silence).
    pub fn peak_out_db(&self) -> f32 {
        gain_to_db(self.peak_out)
    }
}

/// The state the game thread and the audio thread share: settings in, measurements out.
/// Survives an output rebuild (resume): the new limiter effect uses the same block.
#[derive(Debug)]
pub(crate) struct LimiterShared {
    enabled: AtomicBool,
    /// Mono output (ADR-2646): both channels carry `(left + right) / 2` before the limiter.
    mono: AtomicBool,
    /// Ceiling as an amplitude, `f32` bits.
    ceiling: AtomicU32,
    /// Release time constant, seconds, `f32` bits.
    release_secs: AtomicU32,
    frames: AtomicU64,
    limited_frames: AtomicU64,
    nonfinite: AtomicU64,
    /// Non-negative `f32` bits: their integer order is their numeric order, so `fetch_max`
    /// keeps the larger value.
    peak_in: AtomicU32,
    peak_out: AtomicU32,
    max_reduction_db: AtomicU32,
    /// Blocks processed, never reset: voices compare it to tell whether the mixer has run since
    /// they started (`SoundInstance` declicking).
    blocks: Arc<AtomicU64>,
    /// The live program-loudness meter of what leaves the limiter (ADR-7227).
    program: Arc<crate::program_loudness::ProgramShared>,
}

impl Default for LimiterShared {
    fn default() -> Self {
        let shared = Self {
            enabled: AtomicBool::new(true),
            mono: AtomicBool::new(false),
            ceiling: AtomicU32::new(1.0f32.to_bits()),
            release_secs: AtomicU32::new(0.08f32.to_bits()),
            frames: AtomicU64::new(0),
            limited_frames: AtomicU64::new(0),
            nonfinite: AtomicU64::new(0),
            peak_in: AtomicU32::new(0),
            peak_out: AtomicU32::new(0),
            max_reduction_db: AtomicU32::new(0),
            blocks: Arc::new(AtomicU64::new(0)),
            program: Arc::default(),
        };
        shared.set(Some(MasterLimiter::default()));
        shared
    }
}

impl LimiterShared {
    /// Applies new settings; `None` bypasses the gain stage (non-finite samples are still
    /// silenced).
    pub(crate) fn set(&self, limiter: Option<MasterLimiter>) {
        match limiter {
            Some(limiter) => {
                let limiter = limiter.clamped();
                self.ceiling
                    .store(db_to_gain(limiter.ceiling_db).to_bits(), Ordering::Relaxed);
                self.release_secs
                    .store(limiter.release_secs.to_bits(), Ordering::Relaxed);
                self.enabled.store(true, Ordering::Relaxed);
            }
            None => self.enabled.store(false, Ordering::Relaxed),
        }
    }

    /// The settings in effect (`None` = bypassed).
    pub(crate) fn get(&self) -> Option<MasterLimiter> {
        self.enabled.load(Ordering::Relaxed).then(|| MasterLimiter {
            ceiling_db: gain_to_db(f32::from_bits(self.ceiling.load(Ordering::Relaxed))),
            release_secs: f32::from_bits(self.release_secs.load(Ordering::Relaxed)),
        })
    }

    /// Turns the mono downmix on or off (ADR-2646); takes effect at the next audio block.
    pub(crate) fn set_mono(&self, mono: bool) {
        self.mono.store(mono, Ordering::Relaxed);
    }

    /// Whether the output is downmixed to mono.
    pub(crate) fn mono(&self) -> bool {
        self.mono.load(Ordering::Relaxed)
    }

    /// The measurements since the last reset.
    pub(crate) fn meter(&self) -> OutputMeter {
        OutputMeter {
            frames: self.frames.load(Ordering::Relaxed),
            peak_in: f32::from_bits(self.peak_in.load(Ordering::Relaxed)),
            peak_out: f32::from_bits(self.peak_out.load(Ordering::Relaxed)),
            max_reduction_db: f32::from_bits(self.max_reduction_db.load(Ordering::Relaxed)),
            limited_frames: self.limited_frames.load(Ordering::Relaxed),
            nonfinite_samples: self.nonfinite.load(Ordering::Relaxed),
        }
    }

    /// The live program-loudness meter's shared state (ADR-7227).
    pub(crate) fn program(&self) -> &crate::program_loudness::ProgramShared {
        &self.program
    }

    /// The monotonic count of processed blocks.
    pub(crate) fn render_clock(&self) -> &Arc<AtomicU64> {
        &self.blocks
    }

    /// Zeroes the measurements (not the block clock).
    pub(crate) fn reset_meter(&self) {
        self.frames.store(0, Ordering::Relaxed);
        self.limited_frames.store(0, Ordering::Relaxed);
        self.nonfinite.store(0, Ordering::Relaxed);
        self.peak_in.store(0, Ordering::Relaxed);
        self.peak_out.store(0, Ordering::Relaxed);
        self.max_reduction_db.store(0, Ordering::Relaxed);
    }
}

/// Builds the limiter effect for a main track.
pub(crate) struct LimiterBuilder(pub(crate) Arc<LimiterShared>);

impl EffectBuilder for LimiterBuilder {
    type Handle = ();

    fn build(self) -> (Box<dyn Effect>, Self::Handle) {
        (Box::new(LimiterEffect::new(self.0)), ())
    }
}

/// The kira effect. Allocation-free; reads its settings once per block.
pub(crate) struct LimiterEffect {
    shared: Arc<LimiterShared>,
    /// Measures the limited output while the program-loudness meter is on.
    program: crate::program_loudness::ProgramLoudnessTap,
    /// Current gain, `0 < gain ≤ 1`. Kept in `f64`: in `f32` the release stalls about 1.4e-5
    /// under 1 at a 10 ms release (each step is below half an ulp), short of the snap to 1.
    gain: f64,
}

impl LimiterEffect {
    pub(crate) fn new(shared: Arc<LimiterShared>) -> Self {
        let program = crate::program_loudness::ProgramLoudnessTap::with_shared(Arc::clone(&shared.program));
        Self {
            shared,
            program,
            gain: 1.0,
        }
    }

    /// Limits `frames` in place (`dt` = seconds per frame) and folds the block's measurements
    /// into the shared meter.
    pub(crate) fn process_frames(&mut self, frames: &mut [Frame], dt: f64) {
        let shared = &self.shared;
        let enabled = shared.enabled.load(Ordering::Relaxed);
        let mono = shared.mono.load(Ordering::Relaxed);
        let ceiling = f32::from_bits(shared.ceiling.load(Ordering::Relaxed));
        let release = f32::from_bits(shared.release_secs.load(Ordering::Relaxed));
        let coeff = if release > 0.0 {
            (-dt / f64::from(release)).exp()
        } else {
            0.0
        };
        if !enabled {
            self.gain = 1.0;
        }
        let mut peak_in = 0.0f32;
        let mut peak_out = 0.0f32;
        let mut min_gain = 1.0f32;
        let mut limited = 0u64;
        let mut nonfinite = 0u64;
        for frame in frames.iter_mut() {
            if !frame.left.is_finite() {
                frame.left = 0.0;
                nonfinite += 1;
            }
            if !frame.right.is_finite() {
                frame.right = 0.0;
                nonfinite += 1;
            }
            if mono {
                // Equal halves: a centered sound keeps its level, a hard-panned one reaches
                // both ears 6 dB down. The f32 sum of two same-sign large inputs can overflow
                // to +∞ even though both inputs were finite (and opposite infinities average
                // to NaN), so re-check and silence it (counted), like the input check above:
                // an infinite peak here would make the gain stage emit NaN
                // (ceiling/∞ = 0, then ∞·0 = NaN) that kira passes to the device.
                let both = (frame.left + frame.right) * 0.5;
                let both = if both.is_finite() {
                    both
                } else {
                    nonfinite += 1;
                    0.0
                };
                frame.left = both;
                frame.right = both;
            }
            let peak = frame.left.abs().max(frame.right.abs());
            peak_in = peak_in.max(peak);
            if enabled {
                let target = f64::from(if peak > ceiling { ceiling / peak } else { 1.0 });
                self.gain = if target < self.gain {
                    target
                } else {
                    target - (target - self.gain) * coeff
                };
                // The release only approaches 1; land on it once the rest is inaudible so a
                // quiet mix is bit-exact again.
                if target == 1.0 && self.gain > 1.0 - 1e-6 {
                    self.gain = 1.0;
                }
                if self.gain < 1.0 {
                    let gain = self.gain as f32;
                    frame.left *= gain;
                    frame.right *= gain;
                    limited += 1;
                    min_gain = min_gain.min(gain);
                }
            }
            peak_out = peak_out.max(frame.left.abs().max(frame.right.abs()));
        }
        shared.frames.fetch_add(frames.len() as u64, Ordering::Relaxed);
        shared.blocks.fetch_add(1, Ordering::Release);
        if limited > 0 {
            shared.limited_frames.fetch_add(limited, Ordering::Relaxed);
            shared
                .max_reduction_db
                .fetch_max((-gain_to_db(min_gain)).max(0.0).to_bits(), Ordering::Relaxed);
        }
        if nonfinite > 0 {
            shared.nonfinite.fetch_add(nonfinite, Ordering::Relaxed);
        }
        shared.peak_in.fetch_max(peak_in.to_bits(), Ordering::Relaxed);
        shared.peak_out.fetch_max(peak_out.to_bits(), Ordering::Relaxed);
        self.program.process_block(frames, dt);
    }
}

impl Effect for LimiterEffect {
    fn process(&mut self, input: &mut [Frame], dt: f64, _info: &Info) {
        self.process_frames(input, dt);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DT: f64 = 1.0 / 48_000.0;

    fn effect(limiter: Option<MasterLimiter>) -> (LimiterEffect, Arc<LimiterShared>) {
        let shared = Arc::new(LimiterShared::default());
        shared.set(limiter);
        (LimiterEffect::new(Arc::clone(&shared)), shared)
    }

    fn sine(len: usize, amp: f32) -> Vec<Frame> {
        (0..len)
            .map(|i| {
                let s = (i as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * amp;
                Frame::new(s, -s * 0.5)
            })
            .collect()
    }

    #[test]
    fn mono_sums_both_channels_into_each_and_off_is_bit_exact() {
        let (mut limiter, shared) = effect(Some(MasterLimiter::default()));
        let input = vec![
            Frame::new(0.8, 0.0),
            Frame::new(0.0, -0.4),
            Frame::new(0.5, 0.5),
            Frame::new(0.3, -0.3),
        ];
        // Off (the default): untouched.
        assert!(!shared.mono());
        let mut out = input.clone();
        limiter.process_frames(&mut out, DT);
        assert_eq!(out, input);
        // On: each channel is the average; a left-only sound is heard on the right too.
        shared.set_mono(true);
        let mut out = input.clone();
        limiter.process_frames(&mut out, DT);
        let expected = [0.4, -0.2, 0.5, 0.0];
        for (frame, want) in out.iter().zip(expected) {
            assert_eq!(frame.left, frame.right);
            assert!((frame.left - want).abs() < 1e-6, "{frame:?} vs {want}");
        }
        // And back off.
        shared.set_mono(false);
        let mut out = input.clone();
        limiter.process_frames(&mut out, DT);
        assert_eq!(out, input);
    }

    #[test]
    fn mono_works_with_the_limiter_bypassed_and_silences_non_finite_first() {
        let (mut limiter, shared) = effect(None);
        shared.set_mono(true);
        let mut out = vec![Frame::new(f32::NAN, 0.6), Frame::new(1.0, 1.0)];
        limiter.process_frames(&mut out, DT);
        assert_eq!(out[0], Frame::new(0.3, 0.3));
        assert_eq!(out[1], Frame::new(1.0, 1.0));
        assert_eq!(shared.meter().nonfinite_samples, 1);
    }

    #[test]
    fn mono_downmix_overflow_becomes_silence_and_is_counted() {
        // Two same-sign finite channels can overflow f32 in the downmix sum (a crafted or
        // corrupt float32 WAV can carry ±MAX per sample): the manufactured +∞ must not reach
        // the gain stage (where ceiling/∞ = 0 would turn it into NaN) nor the meter's max.
        let (mut limiter, shared) = effect(Some(MasterLimiter::default()));
        shared.set_mono(true);
        let mut out = vec![Frame::new(f32::MAX, f32::MAX)];
        limiter.process_frames(&mut out, DT);
        assert_eq!(out[0], Frame::ZERO, "the overflowed downmix is silenced");
        assert_eq!(
            shared.meter().nonfinite_samples,
            1,
            "the manufactured sample is counted"
        );
        assert_eq!(shared.meter().peak_out, 0.0, "no NaN slips past the meter's max");
        // The same with the gain stage bypassed.
        let (mut bypassed, shared) = effect(None);
        shared.set_mono(true);
        let mut out = vec![Frame::new(f32::MAX, f32::MAX)];
        bypassed.process_frames(&mut out, DT);
        assert_eq!(out[0], Frame::ZERO);
        assert_eq!(shared.meter().nonfinite_samples, 1);
        // Opposite-sign infinities (from the same source) make the average NaN: also silenced.
        let (mut limiter, shared) = effect(Some(MasterLimiter::default()));
        shared.set_mono(true);
        let mut out = vec![Frame::new(f32::INFINITY, f32::NEG_INFINITY)];
        limiter.process_frames(&mut out, DT);
        assert_eq!(out[0], Frame::ZERO, "∞ + −∞ over 2 is NaN: silenced too");
        assert!(shared.meter().nonfinite_samples >= 1);
    }

    #[test]
    fn a_mix_below_the_ceiling_is_bit_exact() {
        let (mut limiter, shared) = effect(Some(MasterLimiter::default()));
        let input = sine(4_800, 0.9);
        let mut out = input.clone();
        limiter.process_frames(&mut out, DT);
        assert_eq!(out, input, "gain 1 below the ceiling");
        let meter = shared.meter();
        assert_eq!(meter.frames, 4_800);
        assert_eq!(meter.limited_frames, 0);
        assert_eq!(meter.max_reduction_db, 0.0);
        assert!((meter.peak_in - 0.9).abs() < 1e-3 && meter.peak_out == meter.peak_in);
    }

    #[test]
    fn overs_never_exceed_the_ceiling_and_keep_their_shape() {
        let limiter_settings = MasterLimiter {
            ceiling_db: -6.0,
            release_secs: 0.05,
        };
        let (mut limiter, shared) = effect(Some(limiter_settings));
        let ceiling = db_to_gain(-6.0);
        let mut out = sine(9_600, 2.0);
        limiter.process_frames(&mut out, DT);
        let peak = out
            .iter()
            .map(|f| f.left.abs().max(f.right.abs()))
            .fold(0.0f32, f32::max);
        assert!(peak <= ceiling * (1.0 + 1e-6), "peak {peak} vs ceiling {ceiling}");
        // No flat top: a hard clip of this sine at the ceiling pins ~46 consecutive samples
        // per half cycle. Without lookahead the limiter pins the rising edge of the first
        // peak; after that only a few samples before each peak touch the ceiling.
        let mut run = 0usize;
        let mut longest = 0usize;
        for f in &out[110..] {
            if f.left.abs() >= ceiling * 0.9999 {
                run += 1;
                longest = longest.max(run);
            } else {
                run = 0;
            }
        }
        assert!(
            longest <= 12,
            "{longest} consecutive samples pinned at the ceiling"
        );
        let meter = shared.meter();
        assert!(meter.limited_frames > 9_000, "{meter:?}");
        assert!(
            (meter.max_reduction_db - 12.0).abs() < 0.2,
            "2.0 into −6 dBFS is ~12 dB of reduction: {meter:?}"
        );
        // The stereo image is kept: the gain is linked across channels.
        for f in &out {
            assert!((f.right + f.left * 0.5).abs() < 1e-5);
        }
    }

    #[test]
    fn the_gain_releases_back_to_exactly_one() {
        let (mut limiter, shared) = effect(Some(MasterLimiter {
            ceiling_db: 0.0,
            release_secs: 0.01,
        }));
        let mut burst = vec![Frame::new(3.0, 3.0); 10];
        limiter.process_frames(&mut burst, DT);
        assert!(burst.iter().all(|f| f.left <= 1.0 + 1e-6));
        // About 14 time constants (480 frames each) later the gain is back at exactly 1 and
        // the signal passes untouched.
        let mut quiet = sine(9_600, 0.5);
        let reference = quiet.clone();
        limiter.process_frames(&mut quiet, DT);
        assert_eq!(quiet[7_000..], reference[7_000..]);
        assert!(quiet[100].left.abs() < reference[100].left.abs() || reference[100].left == 0.0);
        assert!(shared.meter().limited_frames > 10);
    }

    #[test]
    fn non_finite_samples_become_silence_and_are_counted() {
        let (mut limiter, shared) = effect(Some(MasterLimiter::default()));
        let mut frames = vec![
            Frame::new(f32::NAN, 0.25),
            Frame::new(f32::INFINITY, f32::NEG_INFINITY),
            Frame::new(0.5, 0.5),
        ];
        limiter.process_frames(&mut frames, DT);
        assert_eq!(frames[0], Frame::new(0.0, 0.25));
        assert_eq!(frames[1], Frame::ZERO);
        assert_eq!(frames[2], Frame::new(0.5, 0.5), "the limiter is not disturbed");
        assert_eq!(shared.meter().nonfinite_samples, 3);
        // Bypassed, the gain stage is off but non-finite samples are still silenced.
        let (mut bypassed, shared) = effect(None);
        let mut frames = vec![Frame::new(f32::NAN, 4.0)];
        bypassed.process_frames(&mut frames, DT);
        assert_eq!(frames[0], Frame::new(0.0, 4.0));
        assert_eq!(shared.meter().nonfinite_samples, 1);
        assert_eq!(shared.get(), None);
    }

    #[test]
    fn settings_clamp_and_round_trip_and_the_meter_resets() {
        let shared = LimiterShared::default();
        assert_eq!(shared.get(), Some(MasterLimiter::default()));
        shared.set(Some(MasterLimiter {
            ceiling_db: 6.0,
            release_secs: f32::NAN,
        }));
        let got = shared.get().expect("enabled");
        assert!(got.ceiling_db.abs() < 1e-4, "clamped to 0 dBFS: {got:?}");
        assert!((got.release_secs - 0.08).abs() < 1e-6);
        shared.set(Some(MasterLimiter {
            ceiling_db: -40.0,
            release_secs: 9.0,
        }));
        let got = shared.get().expect("enabled");
        assert!((got.ceiling_db - MasterLimiter::MIN_CEILING_DB).abs() < 1e-3);
        assert_eq!(got.release_secs, MasterLimiter::MAX_RELEASE_SECS);

        let mut limiter = LimiterEffect::new(Arc::new(LimiterShared::default()));
        let mut frames = vec![Frame::new(2.0, 0.0)];
        limiter.process_frames(&mut frames, DT);
        let meter = limiter.shared.meter();
        assert_eq!((meter.frames, meter.limited_frames), (1, 1));
        assert!((meter.peak_in_db() - 6.02).abs() < 0.01);
        assert!(meter.peak_out_db().abs() < 0.01);
        limiter.shared.reset_meter();
        assert_eq!(limiter.shared.meter(), OutputMeter::default());
    }

    fn deterministic_stereo_stream(len: usize) -> Vec<Frame> {
        (0..len)
            .map(|i| {
                if i < 32 {
                    return Frame::ZERO;
                }
                let phase = i as f32 * 0.071;
                let (mut left, mut right) = (phase.sin() * 0.62, (phase * 0.73).cos() * 0.47);
                if (40..55).contains(&i) {
                    left *= 4.0;
                    right *= 3.0;
                }
                match i {
                    201 => left = f32::NAN,
                    202 => right = f32::INFINITY,
                    907 => left = f32::NEG_INFINITY,
                    _ => {}
                }
                Frame::new(left, right)
            })
            .collect()
    }

    fn apply_settings_at(shared: &LimiterShared, sample: usize) {
        match sample {
            333 => shared.set(Some(MasterLimiter {
                ceiling_db: -6.0,
                release_secs: 0.12,
            })),
            700 => shared.set(None),
            1_150 => shared.set(Some(MasterLimiter {
                ceiling_db: -3.0,
                release_secs: 0.025,
            })),
            _ => {}
        }
    }

    fn process_partitioned(
        input: &[Frame],
        rate: u32,
        partition_sizes: &[usize],
    ) -> (Vec<Frame>, OutputMeter) {
        let (mut limiter, shared) = effect(Some(MasterLimiter {
            ceiling_db: 0.0,
            release_secs: 0.08,
        }));
        let mut output = input.to_vec();
        let (mut cursor, mut partition) = (0, 0);
        while cursor < output.len() {
            apply_settings_at(&shared, cursor);
            let next_event = [333, 700, 1_150]
                .into_iter()
                .find(|&event| event > cursor)
                .unwrap_or(output.len());
            let count = partition_sizes[partition % partition_sizes.len()]
                .min(next_event - cursor)
                .min(output.len() - cursor);
            limiter.process_frames(&mut output[cursor..cursor + count], 1.0 / f64::from(rate));
            cursor += count;
            partition += 1;
        }
        (output, shared.meter())
    }

    fn assert_meter_close(actual: OutputMeter, expected: OutputMeter) {
        assert_eq!(actual.frames, expected.frames);
        assert_eq!(actual.limited_frames, expected.limited_frames);
        assert_eq!(actual.nonfinite_samples, expected.nonfinite_samples);
        // Atomic peak reductions fold identical per-sample values. Allow one f32 ulp for
        // platform libm differences in dB conversion.
        for (got, want) in [
            (actual.peak_in, expected.peak_in),
            (actual.peak_out, expected.peak_out),
            (actual.max_reduction_db, expected.max_reduction_db),
        ] {
            assert!((got - want).abs() <= 2.0e-6, "{got} vs {want}");
        }
    }

    #[test]
    fn output_and_meter_are_invariant_to_callback_partitioning() {
        const LEN: usize = 1_301;
        let input = deterministic_stereo_stream(LEN);
        let mixed = [3, 17, 64, 129];
        for rate in [44_100, 48_000, 96_000] {
            let (expected, meter) = process_partitioned(&input, rate, &[LEN]);
            for partitions in [&[1][..], &[7][..], &[64][..], &[257][..], &mixed[..]] {
                let (actual, actual_meter) = process_partitioned(&input, rate, partitions);
                for (index, (got, want)) in actual.iter().zip(&expected).enumerate() {
                    assert_eq!(
                        got, want,
                        "rate {rate}, partitions {partitions:?}, sample {index}"
                    );
                }
                assert_meter_close(actual_meter, meter);
            }
        }
    }

    #[test]
    fn resetting_meter_does_not_reset_signal_state() {
        let (mut with_reset, shared) = effect(Some(MasterLimiter {
            ceiling_db: 0.0,
            release_secs: 0.01,
        }));
        let (mut without_reset, _) = effect(Some(MasterLimiter {
            ceiling_db: 0.0,
            release_secs: 0.01,
        }));
        let mut attack_a = vec![Frame::new(3.0, 2.0); 10];
        let mut attack_b = attack_a.clone();
        with_reset.process_frames(&mut attack_a, DT);
        without_reset.process_frames(&mut attack_b, DT);
        shared.reset_meter();
        assert_eq!(shared.meter(), OutputMeter::default());
        let mut tail_a = sine(100, 0.5);
        let mut tail_b = tail_a.clone();
        with_reset.process_frames(&mut tail_a, DT);
        without_reset.process_frames(&mut tail_b, DT);
        assert_eq!(tail_a, tail_b, "meter reset retained the limiter envelope");
        assert_eq!(shared.meter().frames, tail_a.len() as u64);
        assert!(shared.meter().limited_frames > 0);
    }

    #[test]
    fn empty_blocks_do_not_advance_the_gain_envelope() {
        let (mut with_empty, shared) = effect(Some(MasterLimiter {
            ceiling_db: 0.0,
            release_secs: 0.01,
        }));
        let (mut continuous, _) = effect(Some(MasterLimiter {
            ceiling_db: 0.0,
            release_secs: 0.01,
        }));
        let mut attack_a = vec![Frame::new(3.0, 2.0); 10];
        let mut attack_b = attack_a.clone();
        with_empty.process_frames(&mut attack_a, DT);
        continuous.process_frames(&mut attack_b, DT);
        for _ in 0..5 {
            with_empty.process_frames(&mut [], 10.0);
        }
        let mut next_a = vec![Frame::new(0.5, -0.25)];
        let mut next_b = next_a.clone();
        with_empty.process_frames(&mut next_a, DT);
        continuous.process_frames(&mut next_b, DT);
        assert_eq!(next_a, next_b, "empty callbacks retained the gain envelope");
        assert_eq!(shared.meter().frames, 11);
        assert_eq!(shared.render_clock().load(Ordering::Acquire), 7);
    }
}
