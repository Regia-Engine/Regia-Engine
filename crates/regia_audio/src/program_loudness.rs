// SPDX-License-Identifier: MPL-2.0
//! The live program-loudness meter (`docs/specs/audio-loudness.md` § 6, ADR-7227): EBU R128 loudness of
//! the whole output, measured on the audio thread while the game or the play-in-editor world runs.
//!
//! A [`ProgramLoudnessTap`] reads every output block after the master limiter (what the listener
//! hears). It runs the BS.1770-4 K-weighting of [`crate::LoudnessMeter`] (exact at any sample rate) and
//! its true-peak interpolator, sums the K-weighted power over 100 ms hops, and at every hop end
//! publishes, through atomics, the five figures of a loudness meter: momentary (last 4 hops = 400 ms),
//! short-term (last 30 hops = 3 s), integrated (gated), loudness range and true peak. The offline
//! meter builds its 400 ms gating blocks and its short-term readings from the same 100 ms hops, so at a
//! hop boundary the two agree (`tests/program_loudness.rs` holds them to it on a 1 kHz sine, pink noise
//! and a level step).
//!
//! The tap never allocates and never locks: the offline meter keeps every gating block for the
//! integrated value, which grows without bound, so the tap keeps a fixed histogram instead, 0.01 LU
//! bins of block count and summed power from −70 LUFS (the absolute gate) to +30 LUFS. The absolute
//! gate is exact; the relative gate (−10 LU below the mean) keeps the whole bin holding the threshold,
//! so a block within 0.01 LU under it may count. The loudness range uses the offline meter's own
//! 0.1 LU histogram.
//!
//! The meter is off by default (it costs a K-weighting and a true-peak interpolator per channel per
//! sample): the editor's mixer and the agent method `audio.program_loudness` turn it on. Turning it on
//! or resetting it starts a new measurement at the next audio block.

use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
};

use kira::Frame;

use crate::loudness::{
    ABSOLUTE_GATE_LUFS, GATING_BLOCK_SECS, GATING_HOP_SECS, KWeight, LraHistogram, MOMENTARY_SECS,
    RELATIVE_GATE_LU, SHORT_TERM_SECS, TruePeak, lufs_from_power, true_peak_oversampling,
};
use crate::spatial::gain_to_db;

/// Integrated-loudness histogram resolution, LU per bin.
const GATE_STEP_LU: f32 = 0.01;

/// Integrated-loudness histogram bin count: 100 LU above the absolute gate (−70 ..= +30 LUFS) in
/// [`GATE_STEP_LU`] steps.
const GATE_BINS: usize = 10_000;

/// The most hops a window spans (the 3 s short-term window of 100 ms hops).
const MAX_WINDOW_HOPS: usize = 30;

/// One reading of the program-loudness meter. A loudness with no data behind it is `−∞`; the true
/// peak of silence (or of a meter that has seen nothing) is `−100` dBTP; the loudness range is `0.0`
/// with fewer than two short-term readings (as in [`crate::Loudness`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProgramLoudness {
    /// Whether the meter is measuring.
    pub enabled: bool,
    /// Seconds of output measured since the meter was turned on or reset.
    pub seconds: f64,
    /// Momentary loudness (last 400 ms), LUFS.
    pub momentary_lufs: f32,
    /// Short-term loudness (last 3 s), LUFS.
    pub short_term_lufs: f32,
    /// Gated integrated loudness since the meter was turned on or reset, LUFS.
    pub integrated_lufs: f32,
    /// Loudness range since the meter was turned on or reset (EBU Tech 3342), LU.
    pub loudness_range_lu: f32,
    /// Largest true peak since the meter was turned on or reset, dBTP.
    pub true_peak_db: f32,
}

impl Default for ProgramLoudness {
    fn default() -> Self {
        Self::EMPTY
    }
}

impl ProgramLoudness {
    /// The reading of a meter that has measured nothing.
    pub const EMPTY: Self = Self {
        enabled: false,
        seconds: 0.0,
        momentary_lufs: f32::NEG_INFINITY,
        short_term_lufs: f32::NEG_INFINITY,
        integrated_lufs: f32::NEG_INFINITY,
        loudness_range_lu: 0.0,
        true_peak_db: -100.0,
    };
}

/// State shared by a tap (audio thread) and its handles (main thread).
#[derive(Debug)]
pub(crate) struct ProgramShared {
    enabled: AtomicBool,
    /// Set by the main thread; the tap clears its state at the start of its next block.
    reset: AtomicBool,
    frames: AtomicU64,
    rate: AtomicU32,
    momentary: AtomicU32,
    short_term: AtomicU32,
    integrated: AtomicU32,
    range: AtomicU32,
    /// Linear true peak (`f64` bits).
    peak: AtomicU64,
}

impl Default for ProgramShared {
    fn default() -> Self {
        let shared = Self {
            enabled: AtomicBool::new(false),
            reset: AtomicBool::new(false),
            frames: AtomicU64::new(0),
            rate: AtomicU32::new(0),
            momentary: AtomicU32::new(0),
            short_term: AtomicU32::new(0),
            integrated: AtomicU32::new(0),
            range: AtomicU32::new(0),
            peak: AtomicU64::new(0),
        };
        shared.clear_published();
        shared
    }
}

impl ProgramShared {
    fn clear_published(&self) {
        self.frames.store(0, Ordering::Relaxed);
        self.momentary
            .store(f32::NEG_INFINITY.to_bits(), Ordering::Relaxed);
        self.short_term
            .store(f32::NEG_INFINITY.to_bits(), Ordering::Relaxed);
        self.integrated
            .store(f32::NEG_INFINITY.to_bits(), Ordering::Relaxed);
        self.range.store(0.0f32.to_bits(), Ordering::Relaxed);
        self.peak.store(0.0f64.to_bits(), Ordering::Relaxed);
    }

    /// The latest published reading.
    pub(crate) fn read(&self) -> ProgramLoudness {
        let rate = self.rate.load(Ordering::Relaxed);
        let frames = self.frames.load(Ordering::Relaxed);
        ProgramLoudness {
            enabled: self.enabled.load(Ordering::Relaxed),
            seconds: if rate == 0 {
                0.0
            } else {
                frames as f64 / f64::from(rate)
            },
            momentary_lufs: f32::from_bits(self.momentary.load(Ordering::Relaxed)),
            short_term_lufs: f32::from_bits(self.short_term.load(Ordering::Relaxed)),
            integrated_lufs: f32::from_bits(self.integrated.load(Ordering::Relaxed)),
            loudness_range_lu: f32::from_bits(self.range.load(Ordering::Relaxed)),
            true_peak_db: gain_to_db(f64::from_bits(self.peak.load(Ordering::Relaxed)) as f32),
        }
    }

    /// Turns the meter on or off; turning it on starts a new measurement.
    pub(crate) fn set_enabled(&self, enabled: bool) {
        let was = self.enabled.swap(enabled, Ordering::Relaxed);
        if enabled && !was {
            self.reset();
        }
    }

    /// Starts a new measurement: the published figures clear now, the tap's state at its next block.
    pub(crate) fn reset(&self) {
        self.clear_published();
        self.reset.store(true, Ordering::Release);
    }
}

/// The main-thread side of a [`ProgramLoudnessTap`]: cheap to clone, lock-free to read.
#[derive(Clone, Debug)]
pub struct ProgramLoudnessHandle {
    shared: Arc<ProgramShared>,
}

impl ProgramLoudnessHandle {
    /// The latest reading (updated every 100 ms of output).
    #[must_use]
    pub fn read(&self) -> ProgramLoudness {
        self.shared.read()
    }

    /// Turns the meter on (starting a new measurement) or off.
    pub fn set_enabled(&self, enabled: bool) {
        self.shared.set_enabled(enabled);
    }

    /// Starts a new measurement.
    pub fn reset(&self) {
        self.shared.reset();
    }
}

/// The integrated-loudness histogram: count and summed power of the 400 ms gating blocks per
/// 0.01 LU bin, from the absolute gate up.
#[derive(Debug)]
struct GateHistogram {
    counts: Vec<u64>,
    powers: Vec<f64>,
    total_count: u64,
    total_power: f64,
}

impl GateHistogram {
    fn new() -> Self {
        Self {
            counts: vec![0; GATE_BINS],
            powers: vec![0.0; GATE_BINS],
            total_count: 0,
            total_power: 0.0,
        }
    }

    fn clear(&mut self) {
        self.counts.fill(0);
        self.powers.fill(0.0);
        self.total_count = 0;
        self.total_power = 0.0;
    }

    fn bin(lufs: f32) -> usize {
        (((lufs - ABSOLUTE_GATE_LUFS) / GATE_STEP_LU).floor().max(0.0) as usize).min(GATE_BINS - 1)
    }

    /// Adds one gating block (its mean-square `power`); blocks under the absolute gate are dropped.
    fn record(&mut self, power: f64) {
        let lufs = lufs_from_power(power);
        if !lufs.is_finite() || lufs < ABSOLUTE_GATE_LUFS {
            return;
        }
        let bin = Self::bin(lufs);
        self.counts[bin] += 1;
        self.powers[bin] += power;
        self.total_count += 1;
        self.total_power += power;
    }

    /// The gated integrated loudness (BS.1770-4 § 4.1), LUFS; `−∞` without a block.
    fn integrated(&self) -> f32 {
        if self.total_count == 0 {
            return f32::NEG_INFINITY;
        }
        let mean = self.total_power / self.total_count as f64;
        let threshold = lufs_from_power(mean) + RELATIVE_GATE_LU;
        let first = if threshold < ABSOLUTE_GATE_LUFS {
            0
        } else {
            Self::bin(threshold)
        };
        let count: u64 = self.counts[first..].iter().sum();
        let power: f64 = self.powers[first..].iter().sum();
        if count == 0 {
            return f32::NEG_INFINITY;
        }
        lufs_from_power(power / count as f64)
    }
}

/// The audio-thread state of a tap (boxed once: it holds the histograms).
#[derive(Debug)]
struct TapState {
    /// The sample rate the state was set up for (0 = not yet).
    rate: u32,
    filters: [KWeight; 2],
    /// True-peak interpolators below 96 kHz (4×) and at or above it (2×), both built up front so a
    /// rate change does not allocate.
    peaks_4x: [TruePeak; 2],
    peaks_2x: [TruePeak; 2],
    two_x: bool,
    hop_len: usize,
    hop_remaining: usize,
    hop_energy: f64,
    /// Mean K-weighted power of the last hops, newest at `hop_pos - 1`.
    hops: [f64; MAX_WINDOW_HOPS],
    hop_pos: usize,
    hop_count: u64,
    momentary_hops: usize,
    short_hops: usize,
    block_hops: usize,
    peak: f64,
    gate: GateHistogram,
    lra: LraHistogram,
}

impl TapState {
    fn new() -> Self {
        Self {
            rate: 0,
            filters: [KWeight::new(48_000.0), KWeight::new(48_000.0)],
            peaks_4x: [TruePeak::new(48_000), TruePeak::new(48_000)],
            peaks_2x: [TruePeak::new(96_000), TruePeak::new(96_000)],
            two_x: false,
            hop_len: 1,
            hop_remaining: 1,
            hop_energy: 0.0,
            hops: [0.0; MAX_WINDOW_HOPS],
            hop_pos: 0,
            hop_count: 0,
            momentary_hops: 4,
            short_hops: MAX_WINDOW_HOPS,
            block_hops: 4,
            peak: 0.0,
            gate: GateHistogram::new(),
            lra: LraHistogram::new(),
        }
    }

    /// A new measurement at `rate` (no allocation).
    fn restart(&mut self, rate: u32) {
        let hz = f64::from(rate);
        self.rate = rate;
        self.filters = [KWeight::new(hz), KWeight::new(hz)];
        for p in self.peaks_4x.iter_mut().chain(self.peaks_2x.iter_mut()) {
            p.clear();
        }
        self.two_x = true_peak_oversampling(rate) == 2;
        self.hop_len = (GATING_HOP_SECS * hz).round().max(1.0) as usize;
        self.hop_remaining = self.hop_len;
        self.hop_energy = 0.0;
        self.hops = [0.0; MAX_WINDOW_HOPS];
        self.hop_pos = 0;
        self.hop_count = 0;
        let hops_of = |secs: f64| ((secs / GATING_HOP_SECS).round() as usize).clamp(1, MAX_WINDOW_HOPS);
        self.momentary_hops = hops_of(MOMENTARY_SECS);
        self.short_hops = hops_of(SHORT_TERM_SECS);
        self.block_hops = hops_of(GATING_BLOCK_SECS);
        self.peak = 0.0;
        self.gate.clear();
        self.lra.clear();
    }
}

/// The audio-thread side of the program-loudness meter. The mixer runs one inside the master
/// limiter's effect; build one with [`ProgramLoudnessTap::new`] to measure frames yourself (tests,
/// tools).
#[derive(Debug)]
pub struct ProgramLoudnessTap {
    shared: Arc<ProgramShared>,
    state: Box<TapState>,
}

impl ProgramLoudnessTap {
    /// A tap and the handle that reads and controls it. The meter starts off
    /// ([`ProgramLoudnessHandle::set_enabled`]).
    #[must_use]
    pub fn new() -> (Self, ProgramLoudnessHandle) {
        let shared = Arc::new(ProgramShared::default());
        (
            Self::with_shared(Arc::clone(&shared)),
            ProgramLoudnessHandle { shared },
        )
    }

    /// A tap publishing into `shared` (the master limiter's).
    pub(crate) fn with_shared(shared: Arc<ProgramShared>) -> Self {
        Self {
            shared,
            state: Box::new(TapState::new()),
        }
    }

    /// Measures one block of stereo frames; `dt` is the time between frames. Allocation-free; does
    /// nothing while the meter is off.
    pub fn process_block(&mut self, frames: &[Frame], dt: f64) {
        let shared = &*self.shared;
        if !shared.enabled.load(Ordering::Relaxed) || !dt.is_finite() || dt <= 0.0 {
            return;
        }
        let rate = (1.0 / dt).round().clamp(1.0, f64::from(u32::MAX)) as u32;
        let state = &mut *self.state;
        if shared.reset.swap(false, Ordering::Acquire) || rate != state.rate {
            state.restart(rate);
            shared.clear_published();
            shared.rate.store(rate, Ordering::Relaxed);
        }
        let peaks = if state.two_x {
            &mut state.peaks_2x
        } else {
            &mut state.peaks_4x
        };
        let mut hop_ends = 0usize;
        for frame in frames {
            let kl = state.filters[0].process(frame.left);
            let kr = state.filters[1].process(frame.right);
            state.hop_energy += kl * kl + kr * kr;
            for (tp, x) in peaks.iter_mut().zip([frame.left, frame.right]) {
                let x = if x.is_finite() { f64::from(x) } else { 0.0 };
                state.peak = state.peak.max(tp.push(x));
            }
            state.hop_remaining -= 1;
            if state.hop_remaining == 0 {
                hop_ends += 1;
                // `finish_hop` needs the whole state; the interpolators are borrowed above, so
                // finish the hop inline here.
                state.hops[state.hop_pos] = state.hop_energy / state.hop_len as f64;
                state.hop_pos = (state.hop_pos + 1) % MAX_WINDOW_HOPS;
                state.hop_count += 1;
                state.hop_remaining = state.hop_len;
                state.hop_energy = 0.0;
                if state.hop_count >= state.block_hops as u64 {
                    let block = window_of(&state.hops, state.hop_pos, state.hop_count, state.block_hops);
                    state.gate.record(block);
                }
                if state.hop_count >= state.short_hops as u64 {
                    let short = window_of(&state.hops, state.hop_pos, state.hop_count, state.short_hops);
                    state.lra.record(short);
                }
            }
        }
        shared.frames.fetch_add(frames.len() as u64, Ordering::Relaxed);
        if hop_ends > 0 {
            state.publish(shared);
        }
    }
}

/// Mean power of the newest `n` of `count` hops in the ring `hops` (next slot `pos`).
fn window_of(hops: &[f64; MAX_WINDOW_HOPS], pos: usize, count: u64, n: usize) -> f64 {
    let have = count.min(n as u64) as usize;
    if have == 0 {
        return 0.0;
    }
    let sum: f64 = (1..=have)
        .map(|back| hops[(pos + MAX_WINDOW_HOPS - back) % MAX_WINDOW_HOPS])
        .sum();
    sum / have as f64
}

impl TapState {
    /// Publishes the figures after a hop.
    fn publish(&self, shared: &ProgramShared) {
        let momentary = window_of(&self.hops, self.hop_pos, self.hop_count, self.momentary_hops);
        let short = window_of(&self.hops, self.hop_pos, self.hop_count, self.short_hops);
        shared
            .momentary
            .store(lufs_from_power(momentary).to_bits(), Ordering::Relaxed);
        shared
            .short_term
            .store(lufs_from_power(short).to_bits(), Ordering::Relaxed);
        shared
            .integrated
            .store(self.gate.integrated().to_bits(), Ordering::Relaxed);
        shared.range.store(self.lra.range().to_bits(), Ordering::Relaxed);
        shared.peak.store(self.peak.to_bits(), Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_histogram_gates_like_the_block_list() {
        let mut gate = GateHistogram::new();
        assert_eq!(gate.integrated(), f32::NEG_INFINITY);
        // −20 LUFS and −40 LUFS blocks: the relative gate (mean − 10 LU) drops the quiet ones.
        let p = |lufs: f64| 10f64.powf((lufs - crate::loudness::LOUDNESS_OFFSET_DB) / 10.0);
        for _ in 0..10 {
            gate.record(p(-20.0));
            gate.record(p(-40.0));
        }
        gate.record(p(-80.0)); // under the absolute gate
        assert!((gate.integrated() - -20.0).abs() < 1e-4, "{}", gate.integrated());
        // Above the top bin: still counted with its own power.
        let mut loud = GateHistogram::new();
        loud.record(p(45.0));
        assert!((loud.integrated() - 45.0).abs() < 1e-4, "{}", loud.integrated());
    }

    #[test]
    fn a_rate_change_starts_a_new_measurement_without_allocating_state() {
        let (mut tap, handle) = ProgramLoudnessTap::new();
        handle.set_enabled(true);
        let tone = |rate: f64, n: usize| -> Vec<Frame> {
            (0..n)
                .map(|i| {
                    let s = 0.1 * (std::f64::consts::TAU * 1000.0 * i as f64 / rate).sin() as f32;
                    Frame { left: s, right: s }
                })
                .collect()
        };
        tap.process_block(&tone(48_000.0, 48_000), 1.0 / 48_000.0);
        assert!((handle.read().seconds - 1.0).abs() < 1e-9);
        tap.process_block(&tone(96_000.0, 96_000), 1.0 / 96_000.0);
        let r = handle.read();
        assert!((r.seconds - 1.0).abs() < 1e-9, "{r:?}");
        assert!(tap.state.two_x);
        assert!((r.momentary_lufs - -20.0).abs() < 0.1, "{r:?}");
    }
}
