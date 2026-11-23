// SPDX-License-Identifier: MPL-2.0
//! Per-bus level meters: peak, RMS and short-term loudness (`docs/specs/16-audio.md` § 5, ADR-2530).
//!
//! A [`BusMeterTap`] is a kira effect that sits last in a bus's effect chain. On the audio thread
//! it reads each block once, with no allocation and no lock: a fixed 50 ms hop accumulates the
//! block's peak, its sum of squares and its K-weighted power, and at every hop end it publishes
//! three numbers through atomics and pushes the hop's K-weighted power into a fixed ring of 60
//! hops (3 s, the BS.1770 short-term window). The main thread reads the numbers through a
//! [`BusMeterHandle`] without blocking the mixer.
//!
//! The tap sees the bus *before* its fader (kira applies a track's volume after its effects), so
//! the handle also keeps a mirror of the fader level ([`BusMeterHandle::set_fader_db`], fed by
//! `Buses::set_volume`/`set_mute`) and reports levels *post-fader*. The mirror jumps to the
//! target at once: during a fade the reading leads the audible level.
//!
//! The K-weighting filters and the loudness scale are the ones of [`crate::LoudnessMeter`]
//! (BS.1770-4 pre-filter and RLB high-pass, `-0.691` dB offset). The tap does not reuse that
//! meter's struct because it also keeps every 400 ms gating block for the integrated value, which
//! grows without bound; an always-on tap must not allocate. A test compares both on one signal.

use std::sync::{
    Arc,
    atomic::{AtomicU32, AtomicU64, Ordering},
};

use kira::{Frame, effect::Effect, effect::EffectBuilder, info::Info};

/// The documented floor of a displayed meter, dB. Silence reads `-inf` internally and is mapped
/// to this value by [`BusMeterReading::floored`] (and by the agent protocol and the mixer dock).
pub const METER_FLOOR_DB: f32 = -100.0;

/// Meter hop, seconds: the peak and RMS figures update this often.
pub const METER_HOP_SECS: f64 = 0.05;

/// Hops in the short-term window (3 s / 50 ms).
const RING_HOPS: usize = 60;

/// BS.1770 loudness offset, dB (`LUFS = -0.691 + 10 log10(power)`).
const LOUDNESS_OFFSET_DB: f64 = -0.691;

/// One bus's levels, post-fader.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BusMeterReading {
    /// Sample peak over the last hop, dBFS; `-inf` for silence.
    pub peak_db: f32,
    /// RMS over the last hop (both channels together), dBFS; `-inf` for silence.
    pub rms_db: f32,
    /// Short-term (3 s) K-weighted loudness, LUFS; `-inf` for silence.
    pub short_term_lufs: f32,
}

impl BusMeterReading {
    /// A reading of silence.
    pub const SILENT: Self = Self {
        peak_db: f32::NEG_INFINITY,
        rms_db: f32::NEG_INFINITY,
        short_term_lufs: f32::NEG_INFINITY,
    };

    /// The reading with every figure clamped up to [`METER_FLOOR_DB`] (so `-inf` becomes the
    /// floor); what the protocol and the dock show.
    #[must_use]
    pub fn floored(self) -> Self {
        let f = |x: f32| {
            if x.is_nan() {
                METER_FLOOR_DB
            } else {
                x.max(METER_FLOOR_DB)
            }
        };
        Self {
            peak_db: f(self.peak_db),
            rms_db: f(self.rms_db),
            short_term_lufs: f(self.short_term_lufs),
        }
    }
}

/// State shared by a tap (audio thread) and its handles (main thread).
#[derive(Debug)]
struct Shared {
    /// Linear sample peak of the last hop (`f32` bits).
    peak: AtomicU32,
    /// Linear RMS of the last hop (`f32` bits).
    rms: AtomicU32,
    /// Mean K-weighted power over the short-term window (`f64` bits).
    power: AtomicU64,
    /// The bus fader, dB (`f32` bits).
    fader_db: AtomicU32,
}

impl Shared {
    fn new() -> Self {
        Self {
            peak: AtomicU32::new(0),
            rms: AtomicU32::new(0),
            power: AtomicU64::new(0),
            fader_db: AtomicU32::new(0.0f32.to_bits()),
        }
    }
}

fn db_from_linear(x: f64) -> f32 {
    if x <= 0.0 {
        f32::NEG_INFINITY
    } else {
        (20.0 * x.log10()) as f32
    }
}

fn lufs_from_power(power: f64) -> f32 {
    if power <= 0.0 {
        f32::NEG_INFINITY
    } else {
        (LOUDNESS_OFFSET_DB + 10.0 * power.log10()) as f32
    }
}

/// The main-thread side of a bus meter: cheap to clone, lock-free to read.
#[derive(Clone, Debug)]
pub struct BusMeterHandle {
    shared: Arc<Shared>,
}

impl BusMeterHandle {
    /// The latest levels, post-fader: the tap's pre-fader figures plus the fader level.
    #[must_use]
    pub fn read(&self) -> BusMeterReading {
        let fader = f32::from_bits(self.shared.fader_db.load(Ordering::Relaxed));
        let peak = f32::from_bits(self.shared.peak.load(Ordering::Relaxed));
        let rms = f32::from_bits(self.shared.rms.load(Ordering::Relaxed));
        let power = f64::from_bits(self.shared.power.load(Ordering::Relaxed));
        let post = |db: f32| {
            if db.is_finite() {
                db + fader
            } else {
                f32::NEG_INFINITY
            }
        };
        BusMeterReading {
            peak_db: post(db_from_linear(f64::from(peak))),
            rms_db: post(db_from_linear(f64::from(rms))),
            short_term_lufs: post(lufs_from_power(power)),
        }
    }

    /// Mirrors the bus fader (dB) so [`Self::read`] reports post-fader levels. A non-finite
    /// value counts as the mute floor.
    pub fn set_fader_db(&self, db: f32) {
        let db = if db.is_finite() { db } else { crate::bus::MUTE_DB };
        self.shared.fader_db.store(db.to_bits(), Ordering::Relaxed);
    }
}

/// Builds a [`BusMeterTap`] and its [`BusMeterHandle`]; add it as the last effect of a track.
pub struct BusMeterBuilder {
    shared: Arc<Shared>,
}

impl BusMeterBuilder {
    /// A new builder and the handle that will read its tap.
    #[must_use]
    pub fn new() -> (Self, BusMeterHandle) {
        let shared = Arc::new(Shared::new());
        (
            Self {
                shared: Arc::clone(&shared),
            },
            BusMeterHandle { shared },
        )
    }
}

impl BusMeterBuilder {
    /// The tap itself, for driving it without a mixer (tests and tools measuring a buffer).
    #[must_use]
    pub fn into_tap(self) -> BusMeterTap {
        BusMeterTap::new(self.shared)
    }
}

impl EffectBuilder for BusMeterBuilder {
    type Handle = ();

    fn build(self) -> (Box<dyn Effect>, Self::Handle) {
        (Box::new(self.into_tap()), ())
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Biquad {
    b: [f64; 3],
    a: [f64; 2],
    z: [f64; 2],
}

impl Biquad {
    const fn new(c: [f64; 5]) -> Self {
        Self {
            b: [c[0], c[1], c[2]],
            a: [c[3], c[4]],
            z: [0.0; 2],
        }
    }

    #[inline]
    fn process(&mut self, x: f64) -> f64 {
        let y = self.b[0] * x + self.z[0];
        self.z[0] = self.b[1] * x - self.a[0] * y + self.z[1];
        self.z[1] = self.b[2] * x - self.a[1] * y;
        y
    }

    fn clear(&mut self) {
        self.z = [0.0; 2];
    }
}

/// The BS.1770-4 K-weighting (both stages), rebuilt per detected rate from the same
/// prototype math as [`crate::LoudnessMeter`] — the two meters must weight alike at any
/// sample rate, not only at 48 kHz.
#[derive(Clone, Copy, Debug)]
struct KWeight {
    pre: Biquad,
    rlb: Biquad,
}

impl KWeight {
    fn new(rate: f64) -> Self {
        let (pre, rlb) = crate::loudness::k_weight_coefficients(rate);
        Self {
            pre: Biquad::new(pre),
            rlb: Biquad::new(rlb),
        }
    }

    #[inline]
    fn process(&mut self, x: f32) -> f64 {
        if !x.is_finite() {
            self.pre.clear();
            self.rlb.clear();
            return 0.0;
        }
        self.rlb.process(self.pre.process(f64::from(x)))
    }
}

/// The audio-thread side of a bus meter: a kira [`Effect`] that never modifies the audio.
pub struct BusMeterTap {
    shared: Arc<Shared>,
    filters: [KWeight; 2],
    hop_len: usize,
    hop_dt: f64,
    hop_remaining: usize,
    hop_peak: f32,
    hop_sq: f64,
    hop_k: f64,
    ring: [f64; RING_HOPS],
    ring_pos: usize,
    ring_len: usize,
}

impl BusMeterTap {
    fn new(shared: Arc<Shared>) -> Self {
        Self {
            shared,
            // The placeholder rate: the first block's rate-change branch rebuilds both
            // filters at the detected rate before any sample is measured.
            filters: [KWeight::new(48_000.0); 2],
            hop_len: 1,
            hop_dt: 0.0,
            hop_remaining: 1,
            hop_peak: 0.0,
            hop_sq: 0.0,
            hop_k: 0.0,
            ring: [0.0; RING_HOPS],
            ring_pos: 0,
            ring_len: 0,
        }
    }

    /// Measures one block of frames; `dt` is the time between frames. Allocation-free.
    pub fn process_block(&mut self, frames: &[Frame], dt: f64) {
        if dt.is_nan() || dt <= 0.0 {
            return;
        }
        if (dt - self.hop_dt).abs() > f64::EPSILON * 16.0 {
            // First block, or the sample rate changed: a hop is 50 ms of frames, and the
            // K-weighting is rebuilt at the detected rate (like `LoudnessMeter`'s, so both
            // meters weight alike on a 44.1 or 96 kHz device too).
            self.hop_dt = dt;
            self.hop_len = ((METER_HOP_SECS / dt).round() as usize).max(1);
            self.hop_remaining = self.hop_len;
            self.hop_peak = 0.0;
            self.hop_sq = 0.0;
            self.hop_k = 0.0;
            let rate = 1.0 / dt;
            self.filters = [KWeight::new(rate), KWeight::new(rate)];
        }
        for frame in frames {
            let (l, r) = (frame.left, frame.right);
            let kl = self.filters[0].process(l);
            let kr = self.filters[1].process(r);
            self.hop_k += kl * kl + kr * kr;
            if l.is_finite() {
                self.hop_peak = self.hop_peak.max(l.abs());
                self.hop_sq += f64::from(l) * f64::from(l);
            }
            if r.is_finite() {
                self.hop_peak = self.hop_peak.max(r.abs());
                self.hop_sq += f64::from(r) * f64::from(r);
            }
            self.hop_remaining -= 1;
            if self.hop_remaining == 0 {
                self.finish_hop();
            }
        }
    }

    fn finish_hop(&mut self) {
        let n = self.hop_len as f64;
        self.ring[self.ring_pos] = self.hop_k / n;
        self.ring_pos = (self.ring_pos + 1) % RING_HOPS;
        self.ring_len = (self.ring_len + 1).min(RING_HOPS);
        // Summed afresh from the 60 slots each hop: no running-sum drift.
        let mean = self.ring.iter().take(self.ring_len).sum::<f64>() / self.ring_len as f64;
        // Both channels together: RMS of the interleaved samples (2 per frame).
        let rms = (self.hop_sq / (2.0 * n)).sqrt();
        self.shared.peak.store(self.hop_peak.to_bits(), Ordering::Relaxed);
        self.shared.rms.store((rms as f32).to_bits(), Ordering::Relaxed);
        self.shared.power.store(mean.to_bits(), Ordering::Relaxed);
        self.hop_remaining = self.hop_len;
        self.hop_peak = 0.0;
        self.hop_sq = 0.0;
        self.hop_k = 0.0;
    }
}

impl Effect for BusMeterTap {
    fn process(&mut self, input: &mut [Frame], dt: f64, _info: &Info) {
        self.process_block(input, dt);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loudness::LoudnessMeter;

    const RATE: u32 = 48_000;

    fn tap() -> (BusMeterTap, BusMeterHandle) {
        let (builder, handle) = BusMeterBuilder::new();
        (builder.into_tap(), handle)
    }

    fn sine(amp: f32, hz: f32, frames: usize, start: usize) -> Vec<Frame> {
        (0..frames)
            .map(|i| {
                let t = (start + i) as f32 / RATE as f32;
                let s = amp * (std::f32::consts::TAU * hz * t).sin();
                Frame { left: s, right: s }
            })
            .collect()
    }

    #[test]
    fn sine_peak_and_rms_match_analytic_within_a_tenth_of_a_db() {
        let (mut tap, handle) = tap();
        let amp = 0.5f32;
        let dt = 1.0 / f64::from(RATE);
        // 300 ms in 256-frame blocks (a block is not a whole number of hops).
        let all = sine(amp, 1000.0, 14_400, 0);
        for block in all.chunks(256) {
            tap.process_block(block, dt);
        }
        let r = handle.read();
        let peak = 20.0 * amp.log10();
        let rms = 20.0 * (amp / std::f32::consts::SQRT_2).log10();
        assert!((r.peak_db - peak).abs() < 0.1, "peak {} vs {peak}", r.peak_db);
        assert!((r.rms_db - rms).abs() < 0.1, "rms {} vs {rms}", r.rms_db);
    }

    #[test]
    fn fader_mirror_makes_the_reading_post_fader() {
        let (mut tap, handle) = tap();
        let dt = 1.0 / f64::from(RATE);
        tap.process_block(&sine(0.5, 1000.0, 4800, 0), dt);
        let flat = handle.read();
        handle.set_fader_db(-6.0);
        let low = handle.read();
        assert!((low.peak_db - (flat.peak_db - 6.0)).abs() < 1e-4);
        handle.set_fader_db(crate::bus::MUTE_DB);
        assert!(handle.read().peak_db < -60.0);
    }

    #[test]
    fn silence_reads_negative_infinity_and_floors_to_the_documented_value() {
        let (mut tap, handle) = tap();
        assert_eq!(handle.read(), BusMeterReading::SILENT);
        let silence = vec![Frame::ZERO; 4800];
        tap.process_block(&silence, 1.0 / f64::from(RATE));
        let r = handle.read();
        assert_eq!(r.peak_db, f32::NEG_INFINITY);
        assert_eq!(r.rms_db, f32::NEG_INFINITY);
        assert_eq!(r.short_term_lufs, f32::NEG_INFINITY);
        let f = r.floored();
        assert_eq!(f.peak_db, METER_FLOOR_DB);
        assert_eq!(f.rms_db, METER_FLOOR_DB);
        assert_eq!(f.short_term_lufs, METER_FLOOR_DB);
    }

    #[test]
    fn short_term_loudness_matches_the_loudness_meter() {
        // Deep-zai-audio-mix finding 6: the tap's K-weighting tracks the rate it detects from
        // `dt`, exactly like `LoudnessMeter` — a 40 Hz tone sits on the RLB corner, where a
        // wrong-rate filter reads several tenths of a LU off.
        for rate in [44_100, 48_000, 96_000] {
            let (mut tap, handle) = tap();
            let dt = 1.0 / f64::from(rate);
            let frames = 4 * rate as usize;
            let all: Vec<Frame> = (0..frames)
                .map(|i| {
                    let t = i as f32 / rate as f32;
                    let s = 0.25 * (std::f32::consts::TAU * 40.0 * t).sin();
                    Frame { left: s, right: s }
                })
                .collect();
            for block in all.chunks(512) {
                tap.process_block(block, dt);
            }
            let mut meter = LoudnessMeter::new(2, rate).expect("stereo meter");
            let inter: Vec<f32> = all.iter().flat_map(|f| [f.left, f.right]).collect();
            meter.push(&inter);
            let want = meter.short_term_lufs();
            let got = handle.read().short_term_lufs;
            assert!(
                (got - want).abs() < 0.1,
                "rate {rate}: tap {got} LUFS vs meter {want} LUFS"
            );
        }
    }

    #[test]
    fn non_finite_samples_do_not_poison_the_meter() {
        let (mut tap, handle) = tap();
        let dt = 1.0 / f64::from(RATE);
        let mut block = sine(0.5, 1000.0, 4800, 0);
        block[10] = Frame {
            left: f32::NAN,
            right: f32::INFINITY,
        };
        tap.process_block(&block, dt);
        tap.process_block(&sine(0.5, 1000.0, 4800, 0), dt);
        let r = handle.read();
        assert!(r.peak_db.is_finite() && r.rms_db.is_finite() && r.short_term_lufs.is_finite());
    }
}
