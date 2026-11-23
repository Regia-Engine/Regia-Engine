// SPDX-License-Identifier: MPL-2.0
//! Granular synthesis for the granular patch layer (`.audiopatch.ron` v2, ADR-7226,
//! `docs/specs/16-audio.md` § 3a).
//!
//! A grain is a short slice of a source sound, shaped by a periodic Hann window
//! `w(a) = 0.5 - 0.5 cos(2π a / G)` over its `G` frames, read from a start position at a playback
//! rate (the layer's pitch) with linear interpolation, the source treated as looping. Grains start
//! every `sample_rate / density` frames (the first one on the first frame), so with
//! `G = 2 · interval` neighbouring grains overlap by half and their windows sum to exactly 1: a
//! steady source plays back at its own level. Each grain reads its position, rate and the next
//! interval when it starts; a change never jumps inside a playing grain. The optional start jitter
//! comes from a seeded SplitMix64 sequence, so a render is reproducible.
//!
//! Sources: granular synthesis after Roads, *Microsound* (MIT Press, 2001), ch. 3 (grain envelope,
//! density, the overlap of synchronous grains); the periodic Hann window and its overlap-add
//! property after Harris, "On the use of windows for harmonic analysis with the discrete Fourier
//! transform", Proc. IEEE 66 (1978); SplitMix64 after Steele, Lea and Flood, "Fast splittable
//! pseudorandom number generators", OOPSLA 2014. Original code (`docs/provenance.md`).
//!
//! [`GranularCore`] is the pure engine (tests drive it sample by sample); [`GranularSoundData`]
//! wraps it as a kira sound whose controls ([`GranularHandle`]) are atomics the game thread
//! writes every tick. The audio thread never allocates: the grain pool is built up front.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use kira::{
    Frame,
    info::Info,
    sound::{Sound, SoundData},
};

/// The most grains sounding at once; a grain due while the pool is full is skipped (counted in
/// [`GranularHandle::grains_dropped`]).
pub const MAX_GRAINS: usize = 64;

/// Frames over which a gain change is ramped (about 2.7 ms at 48 kHz): no zipper noise.
const GAIN_RAMP_FRAMES: u32 = 128;

/// The grain controls read when a grain starts (and the output gain, ramped).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GranularControls {
    /// Start position of the next grain, `0..1` of the source length.
    pub position: f32,
    /// Playback rate of the next grain (1 = original pitch).
    pub pitch: f32,
    /// Grains per second.
    pub density_hz: f32,
    /// Linear output gain.
    pub gain: f32,
}

impl Default for GranularControls {
    fn default() -> Self {
        Self {
            position: 0.0,
            pitch: 1.0,
            density_hz: 40.0,
            gain: 1.0,
        }
    }
}

/// The periodic Hann window value of frame `age` of a grain of `len` frames.
pub fn hann(age: usize, len: usize) -> f32 {
    if len == 0 {
        return 0.0;
    }
    let phase = std::f64::consts::TAU * age as f64 / len as f64;
    (0.5 - 0.5 * phase.cos()) as f32
}

/// The source frame at fractional `position`, looping, linearly interpolated (exact on whole
/// frames). `source` must not be empty.
fn sample(source: &[Frame], position: f64) -> Frame {
    let len = source.len();
    let floor = position.floor();
    let frac = (position - floor) as f32;
    let i0 = (floor as i64).rem_euclid(len as i64) as usize;
    let a = source[i0];
    if frac == 0.0 {
        return a;
    }
    let b = source[(i0 + 1) % len];
    a * (1.0 - frac) + b * frac
}

/// One SplitMix64 step: the next state and its output.
fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[derive(Clone, Copy, Debug, Default)]
struct Grain {
    active: bool,
    read: f64,
    step: f64,
    age: usize,
}

/// The pure granular engine: a looping source, a grain length and a pool of grains.
pub struct GranularCore {
    source: Arc<[Frame]>,
    sample_rate: u32,
    grain_len: usize,
    jitter: f32,
    rng: u64,
    grains: Vec<Grain>,
    /// Frames until the next grain starts (fractional intervals carry over).
    until_next: f64,
    gain_now: f32,
    gain_from: f32,
    gain_to: f32,
    ramp_left: u32,
    started: u64,
    dropped: u64,
}

impl GranularCore {
    /// A core reading `source` (frames at `sample_rate`) with grains of `grain_secs`, a start
    /// jitter of `± jitter` of the source length, seeded with `seed`, at initial gain `gain`.
    /// An empty source renders silence.
    pub fn new(
        source: Arc<[Frame]>,
        sample_rate: u32,
        grain_secs: f32,
        jitter: f32,
        seed: u64,
        gain: f32,
    ) -> Self {
        let grain_len = (f64::from(grain_secs.max(0.0)) * f64::from(sample_rate))
            .round()
            .max(1.0) as usize;
        let gain = if gain.is_finite() { gain.max(0.0) } else { 0.0 };
        Self {
            source,
            sample_rate: sample_rate.max(1),
            grain_len,
            jitter: if jitter.is_finite() {
                jitter.clamp(0.0, 1.0)
            } else {
                0.0
            },
            rng: seed,
            grains: vec![Grain::default(); MAX_GRAINS],
            until_next: 0.0,
            gain_now: gain,
            gain_from: gain,
            gain_to: gain,
            ramp_left: 0,
            started: 0,
            dropped: 0,
        }
    }

    /// Grain length in frames.
    pub fn grain_len(&self) -> usize {
        self.grain_len
    }

    /// Grains started so far.
    pub fn grains_started(&self) -> u64 {
        self.started
    }

    /// Grains skipped because the pool was full.
    pub fn grains_dropped(&self) -> u64 {
        self.dropped
    }

    /// Grains sounding now.
    pub fn active_grains(&self) -> usize {
        self.grains.iter().filter(|g| g.active).count()
    }

    fn start_grain(&mut self, controls: &GranularControls) {
        let len = self.source.len() as f64;
        let position = if controls.position.is_finite() {
            controls.position.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let mut start = (f64::from(position) * len).floor();
        if self.jitter > 0.0 {
            // A uniform offset in [-jitter, +jitter] of the source length, whole frames.
            let unit = (splitmix64(&mut self.rng) >> 11) as f64 / (1u64 << 53) as f64;
            start += ((unit * 2.0 - 1.0) * f64::from(self.jitter) * len).round();
        }
        let pitch = if controls.pitch.is_finite() {
            controls.pitch.max(0.0)
        } else {
            1.0
        };
        match self.grains.iter_mut().find(|g| !g.active) {
            Some(slot) => {
                *slot = Grain {
                    active: true,
                    read: start.rem_euclid(len),
                    step: f64::from(pitch),
                    age: 0,
                };
                self.started += 1;
            }
            None => self.dropped += 1,
        }
    }

    /// Renders `out.len()` frames with `controls` (the gain ramps to `controls.gain` over a few
    /// milliseconds; the other controls apply to grains starting in this block).
    pub fn render(&mut self, controls: &GranularControls, out: &mut [Frame]) {
        let target = if controls.gain.is_finite() {
            controls.gain.max(0.0)
        } else {
            0.0
        };
        if target != self.gain_to {
            self.gain_from = self.gain_now;
            self.gain_to = target;
            self.ramp_left = GAIN_RAMP_FRAMES;
        }
        if self.source.is_empty() {
            out.fill(Frame::ZERO);
            return;
        }
        // At most one grain per frame: a huge density cannot spin the loop below.
        let density = if controls.density_hz.is_finite() {
            controls.density_hz.clamp(0.1, self.sample_rate as f32)
        } else {
            0.1
        };
        let interval = f64::from(self.sample_rate) / f64::from(density);
        for slot in out.iter_mut() {
            while self.until_next <= 0.0 {
                self.start_grain(controls);
                self.until_next += interval;
            }
            let mut sum = Frame::ZERO;
            let grain_len = self.grain_len;
            for grain in self.grains.iter_mut().filter(|g| g.active) {
                sum += sample(&self.source, grain.read) * hann(grain.age, grain_len);
                grain.age += 1;
                grain.read += grain.step;
                if grain.age >= grain_len {
                    grain.active = false;
                }
            }
            if self.ramp_left > 0 {
                self.ramp_left -= 1;
                let t = 1.0 - self.ramp_left as f32 / GAIN_RAMP_FRAMES as f32;
                self.gain_now = self.gain_from + (self.gain_to - self.gain_from) * t;
            }
            *slot = sum * self.gain_now;
            self.until_next -= 1.0;
        }
    }
}

/// Shared between the game thread ([`GranularHandle`]) and the audio thread.
struct GranularShared {
    position: AtomicU32,
    pitch: AtomicU32,
    density: AtomicU32,
    gain: AtomicU32,
    stop: AtomicBool,
    finished: AtomicBool,
    started: AtomicU64,
    dropped: AtomicU64,
}

impl GranularShared {
    fn controls(&self) -> GranularControls {
        let load = |a: &AtomicU32| f32::from_bits(a.load(Ordering::Relaxed));
        GranularControls {
            position: load(&self.position),
            pitch: load(&self.pitch),
            density_hz: load(&self.density),
            gain: load(&self.gain),
        }
    }
}

/// A granular voice to play on a kira track ([`GranularCore`] behind atomics).
pub struct GranularSoundData {
    core: GranularCore,
    controls: GranularControls,
    fade_frames: usize,
}

impl GranularSoundData {
    /// A voice reading `source` (frames at `sample_rate`, the mixer's rate) with grains of
    /// `grain_secs`, `± jitter` start jitter from `seed`, starting with `controls`.
    pub fn new(
        source: Arc<[Frame]>,
        sample_rate: u32,
        grain_secs: f32,
        jitter: f32,
        seed: u64,
        controls: GranularControls,
    ) -> Self {
        let fade_frames = (crate::DECLICK_FADE.as_secs_f64() * f64::from(sample_rate)).ceil() as usize;
        Self {
            core: GranularCore::new(source, sample_rate, grain_secs, jitter, seed, controls.gain),
            controls,
            fade_frames: fade_frames.max(1),
        }
    }
}

impl SoundData for GranularSoundData {
    type Error = std::convert::Infallible;
    type Handle = GranularHandle;

    fn into_sound(self) -> Result<(Box<dyn Sound>, Self::Handle), Self::Error> {
        let bits = |v: f32| AtomicU32::new(v.to_bits());
        let shared = Arc::new(GranularShared {
            position: bits(self.controls.position),
            pitch: bits(self.controls.pitch),
            density: bits(self.controls.density_hz),
            gain: bits(self.controls.gain),
            stop: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            started: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        });
        let sound = GranularSound {
            core: self.core,
            shared: Arc::clone(&shared),
            fade_left: self.fade_frames,
            fade_len: self.fade_frames,
            done: false,
        };
        Ok((Box::new(sound), GranularHandle { shared }))
    }
}

/// Controls a playing granular voice from the game thread.
pub struct GranularHandle {
    shared: Arc<GranularShared>,
}

impl GranularHandle {
    /// Sets the controls grains read when they start (gain ramps).
    pub fn set_controls(&self, controls: GranularControls) {
        let store = |a: &AtomicU32, v: f32| a.store(v.to_bits(), Ordering::Relaxed);
        store(&self.shared.position, controls.position);
        store(&self.shared.pitch, controls.pitch);
        store(&self.shared.density, controls.density_hz);
        store(&self.shared.gain, controls.gain);
    }

    /// The controls last set.
    pub fn controls(&self) -> GranularControls {
        self.shared.controls()
    }

    /// Fades the voice out over [`crate::DECLICK_FADE`] and ends it.
    pub fn stop(&self) {
        self.shared.stop.store(true, Ordering::Relaxed);
    }

    /// True once the voice has ended after [`Self::stop`].
    pub fn finished(&self) -> bool {
        self.shared.finished.load(Ordering::Relaxed)
    }

    /// Grains started so far (published after each rendered block).
    pub fn grains_started(&self) -> u64 {
        self.shared.started.load(Ordering::Relaxed)
    }

    /// Grains skipped because [`MAX_GRAINS`] were sounding.
    pub fn grains_dropped(&self) -> u64 {
        self.shared.dropped.load(Ordering::Relaxed)
    }
}

/// The kira side of [`GranularSoundData`].
struct GranularSound {
    core: GranularCore,
    shared: Arc<GranularShared>,
    fade_left: usize,
    fade_len: usize,
    done: bool,
}

impl Sound for GranularSound {
    fn process(&mut self, out: &mut [Frame], _dt: f64, _info: &Info) {
        if self.done {
            out.fill(Frame::ZERO);
            return;
        }
        let controls = self.shared.controls();
        self.core.render(&controls, out);
        if self.shared.stop.load(Ordering::Relaxed) {
            for slot in out.iter_mut() {
                if self.fade_left == 0 {
                    *slot = Frame::ZERO;
                    continue;
                }
                *slot *= self.fade_left as f32 / self.fade_len as f32;
                self.fade_left -= 1;
            }
            if self.fade_left == 0 {
                self.done = true;
                self.shared.finished.store(true, Ordering::Relaxed);
            }
        }
        self.shared
            .started
            .store(self.core.grains_started(), Ordering::Relaxed);
        self.shared
            .dropped
            .store(self.core.grains_dropped(), Ordering::Relaxed);
    }

    fn finished(&self) -> bool {
        self.done
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mono(samples: &[f32]) -> Arc<[Frame]> {
        samples.iter().map(|&s| Frame::from_mono(s)).collect()
    }

    /// The reference, written out from the definition in the module docs (not from the engine):
    /// grain `k` starts at frame `k · interval` and adds `w(n - t_k) · src[(start + n - t_k) mod len]`.
    fn reference(
        src: &[f32],
        start: usize,
        grain: usize,
        interval: usize,
        frames: usize,
        gain: f32,
    ) -> Vec<f32> {
        let mut out = vec![0.0f32; frames];
        let mut onset = 0usize;
        while onset < frames {
            for age in 0..grain {
                let n = onset + age;
                if n >= frames {
                    break;
                }
                let w = (0.5 - 0.5 * (std::f64::consts::TAU * age as f64 / grain as f64).cos()) as f32;
                out[n] += src[(start + age) % src.len()] * w;
            }
            onset += interval;
        }
        out.iter().map(|s| s * gain).collect()
    }

    #[test]
    fn grains_match_the_hand_computed_overlap_add_sample_for_sample() {
        // 1 kHz rate keeps the numbers small: 10-frame grains every 5 frames (half overlap).
        let src: Vec<f32> = (0..37).map(|i| ((i * 7) % 13) as f32 / 13.0 - 0.5).collect();
        let mut core = GranularCore::new(mono(&src), 1_000, 0.010, 0.0, 0, 0.5);
        let controls = GranularControls {
            position: 4.0 / 37.0 + 1e-6,
            pitch: 1.0,
            density_hz: 200.0,
            gain: 0.5,
        };
        let mut out = vec![Frame::ZERO; 120];
        core.render(&controls, &mut out[..50]);
        core.render(&controls, &mut out[50..]);
        let expected = reference(&src, 4, 10, 5, 120, 0.5);
        for (n, (got, want)) in out.iter().zip(&expected).enumerate() {
            assert!(
                (got.left - want).abs() <= 1e-6 && got.left == got.right,
                "frame {n}: {} vs {want}",
                got.left
            );
        }
        assert_eq!(
            core.grains_started(),
            24,
            "one grain every 5 frames over 120 frames"
        );
    }

    #[test]
    fn half_overlapped_hann_grains_sum_to_one() {
        let src = vec![0.25f32; 64];
        let mut core = GranularCore::new(mono(&src), 48_000, 0.002, 0.0, 0, 1.0);
        let len = core.grain_len();
        assert_eq!(len, 96);
        let controls = GranularControls {
            density_hz: 48_000.0 / 48.0,
            ..GranularControls::default()
        };
        let mut out = vec![Frame::ZERO; 960];
        core.render(&controls, &mut out);
        // After the first half grain the windows sum to 1, so the output is the source level.
        for (n, frame) in out.iter().enumerate().skip(len / 2) {
            assert!((frame.left - 0.25).abs() < 1e-6, "frame {n}: {}", frame.left);
        }
    }

    #[test]
    fn pitch_two_reads_every_other_source_frame() {
        let src: Vec<f32> = (0..100).map(|i| i as f32 / 100.0).collect();
        let mut core = GranularCore::new(mono(&src), 1_000, 0.008, 0.0, 0, 1.0);
        let controls = GranularControls {
            position: 0.0,
            pitch: 2.0,
            density_hz: 1.0, // one grain in the render
            gain: 1.0,
        };
        let mut out = vec![Frame::ZERO; 8];
        core.render(&controls, &mut out);
        for (age, frame) in out.iter().enumerate() {
            let want = src[2 * age] * hann(age, 8);
            assert!((frame.left - want).abs() <= 1e-7, "age {age}");
        }
    }

    #[test]
    fn jitter_is_reproducible_and_seed_dependent() {
        let src: Vec<f32> = (0..500).map(|i| (i as f32 * 0.37).sin()).collect();
        let render = |seed: u64| {
            let mut core = GranularCore::new(mono(&src), 8_000, 0.004, 0.3, seed, 1.0);
            let mut out = vec![Frame::ZERO; 400];
            core.render(&GranularControls::default(), &mut out);
            out.iter().map(|f| f.left).collect::<Vec<f32>>()
        };
        assert_eq!(render(7), render(7), "same seed, same grains");
        assert_ne!(render(7), render(8), "another seed moves the grains");
    }

    #[test]
    fn a_full_pool_drops_grains_instead_of_allocating() {
        let src = vec![0.1f32; 10];
        // 1-second grains every frame: the pool fills after MAX_GRAINS frames.
        let mut core = GranularCore::new(mono(&src), 1_000, 1.0, 0.0, 0, 1.0);
        let controls = GranularControls {
            density_hz: 1_000.0,
            ..GranularControls::default()
        };
        let mut out = vec![Frame::ZERO; 100];
        core.render(&controls, &mut out);
        assert_eq!(core.grains_started(), MAX_GRAINS as u64);
        assert_eq!(core.grains_dropped(), 100 - MAX_GRAINS as u64);
        assert!(out.iter().all(|f| f.left.is_finite()));
    }

    #[test]
    fn an_empty_source_and_bad_controls_render_finite_silence() {
        let mut core = GranularCore::new(Arc::from(Vec::<Frame>::new()), 48_000, 0.05, 0.0, 0, 1.0);
        let mut out = vec![Frame::from_mono(9.0); 16];
        core.render(&GranularControls::default(), &mut out);
        assert!(out.iter().all(|f| f.left == 0.0 && f.right == 0.0));
        let mut core = GranularCore::new(mono(&[0.5; 32]), 48_000, 0.001, 0.0, 0, 1.0);
        let bad = GranularControls {
            position: f32::NAN,
            pitch: f32::INFINITY,
            density_hz: f32::NAN,
            gain: f32::NAN,
        };
        core.render(&bad, &mut out);
        assert!(out.iter().all(|f| f.left.is_finite()));
    }
}
