// SPDX-License-Identifier: MIT
// Portions ported from hrtf (https://github.com/mrDIMAS/hrtf, commit 2822d67, crates.io release
// 0.9.0), Copyright (c) 2020 Dmitry Stepanov, MIT licence.
//
// Permission is hereby granted, free of charge, to any person obtaining a copy of this software
// and associated documentation files (the "Software"), to deal in the Software without
// restriction, including without limitation the rights to use, copy, modify, merge, publish,
// distribute, sublicense, and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all copies or
// substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING
// BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND
// NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM,
// DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.

//! Binaural rendering for headphones: head-related transfer functions (HRTF).
//!
//! An emitter heard by a listener with [`AudioListener::hrtf`](crate::AudioListener::hrtf) plays
//! through a per-emitter child track whose last effect is an [`HrtfEffectBuilder`]: the voice is
//! folded to mono and convolved with the left- and right-ear impulse responses (HRIRs) of the
//! emitter's direction in the listener's head frame. Interaural time and level differences and
//! the pinna's spectral notches let the ear place the sound — in front or behind, above or
//! below — which stereo panning cannot.
//!
//! # Where the pieces come from
//!
//! - **Sphere interpolation and convolution** are adapted from the MIT-licensed `hrtf` crate
//!   (Dmitry Stepanov; the processor fyrox-sound's HRTF renderer drives): a triangulated sphere
//!   of HRIR pairs in its `.hrir` file format, barycentric interpolation of the HRIR spectra of
//!   the face a direction passes through, overlap-save FFT convolution, and per-sub-block
//!   interpolation of the direction (click-free motion). Regia Engine's adaptation shares one
//!   spectra table between all emitters (the crate copies it per source), transforms the mono
//!   input once for both ears, finds faces with a cached-face search instead of a BSP, and
//!   loads spheres without the crate's resampling pass, which shifts short HRIRs by its filter
//!   delay and truncates them (measured: a 97-tap synthesized sphere lost its interaural delay).
//!   See `docs/ported-sources.toml`.
//! - **The HRIR sphere**: no measured data set is shipped (measured sets carry their own
//!   licences). [`HrirSphere::synthesized`] builds one from the structural model of C. P. Brown
//!   and R. O. Duda, "A structural model for binaural sound synthesis", IEEE Trans. Speech and
//!   Audio Processing 6(5), 1998: a spherical-head shadow (one-pole/one-zero filter per ear,
//!   eqs. 2–3), the Woodworth/Schlosberg interaural delay (eq. 4) and five pinna echoes whose
//!   delays depend on azimuth and elevation (eq. 5, Table I). Projects that want a measured
//!   head load a `.hrir` sphere file with [`crate::Audio::load_hrir_sphere`].
//!
//! # Coordinate convention
//!
//! Sphere points are source directions in the listener's head frame: +X right ear, +Y up,
//! −Z straight ahead (Regia Engine's camera convention). A measured sphere in another convention must
//! be rotated with [`HrirSphere::rotate`] after loading.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use kira::{
    Frame,
    effect::{Effect, EffectBuilder},
    info::Info,
};
use regia_core::math::{Quat, Vec3};
use rustfft::{Fft, FftPlanner, num_complex::Complex};

/// The sample rate the synthesized sphere is built at, and the rate of a sphere's first table.
/// [`HrirSphere::spectra`] also builds a table resampled to [`HRIR_ALT_SAMPLE_RATE`]; a table
/// for any other mixer rate is added on first need with [`SphereSpectra::with_rate`]
/// (ADR-7461). The binaural effect uses the table at the rate kira runs it at, or the nearest
/// one while that table does not exist yet (ADR-2721; before it a 44.1 kHz device heard the
/// 48 kHz HRIRs with interaural delays 9 % long, and before ADR-7461 a 96 kHz device heard
/// them with half the delay).
pub const HRIR_SAMPLE_RATE: u32 = 48_000;

/// The second rate every sphere's spectra are prepared at (the other common device rate).
pub const HRIR_ALT_SAMPLE_RATE: u32 = 44_100;

/// Most rate tables one [`SphereSpectra`] holds: the two built up front ([`HRIR_SAMPLE_RATE`],
/// [`HRIR_ALT_SAMPLE_RATE`]) and up to four more built on first need for other mixer rates
/// (88.2, 96, 176.4 and 192 kHz in practice). Past the cap a mixer uses the nearest table
/// (ADR-7461).
pub const HRTF_MAX_RATE_TABLES: usize = 6;

/// Taps per synthesized HRIR (2 ms at 48 kHz: interaural delay ≤ 0.66 ms, pinna echoes
/// ≤ 0.35 ms, the shadow filter's tail and the fractional-delay kernel). With
/// [`HRTF_BLOCK_LEN`] the FFT size is `32 + 97 − 1 = 128`.
pub const HRIR_LEN: usize = 97;

/// Samples per convolution sub-block; the direction is interpolated once per sub-block.
pub const HRTF_BLOCK_LEN: usize = 32;

/// Sub-blocks per processed chunk (the effect's latency is one chunk: 128 samples, 2.7 ms).
pub const HRTF_INTERPOLATION_STEPS: usize = 4;

const CHUNK: usize = HRTF_BLOCK_LEN * HRTF_INTERPOLATION_STEPS;

/// Head radius, metres (Brown & Duda's average adult head).
const HEAD_RADIUS: f32 = 0.0875;
/// Speed of sound for the head model, m/s.
const SOUND_SPEED: f32 = 343.0;
/// Head-shadow minimum `α` and the angle where it is reached (Brown & Duda eq. 3).
const ALPHA_MIN: f32 = 0.1;
const THETA_MIN: f32 = 150.0 * std::f32::consts::PI / 180.0;
/// Pinna echoes (Brown & Duda Table I): reflection coefficient ρ, and delay terms A, B (samples
/// at 44.1 kHz) and D.
const PINNA: [(f32, f32, f32, f32); 5] = [
    (0.5, 1.0, 2.0, 1.0),
    (-1.0, 5.0, 4.0, 0.5),
    (0.5, 5.0, 7.0, 0.5),
    (-0.25, 5.0, 11.0, 0.5),
    (0.25, 5.0, 13.0, 0.5),
];
/// Half-width of the windowed-sinc fractional delay kernel, samples. Every HRIR is delayed by
/// this much so the kernel is never truncated.
const SINC_HALF: usize = 8;

/// Shared, lock-free control of one emitter's binaural effect: the emitter's head-frame
/// direction and the spatial blend, written by the game thread every sync, read by the audio
/// thread every chunk. The four values are published together (a sequence counter around the
/// stores), so the audio thread never mixes two directions.
///
/// The effect reports back the other way: the rate kira runs it at and the rate of the HRIR
/// table it convolves with ([`Self::mixer_rate`], [`Self::table_rate`]), so the audio system
/// can see a mixer that has no table at its rate yet (ADR-7461).
#[derive(Debug)]
pub struct HrtfControl {
    /// Odd while [`Self::set`] is storing; bumped twice per set.
    seq: AtomicU32,
    x: AtomicU32,
    y: AtomicU32,
    z: AtomicU32,
    blend: AtomicU32,
    /// The rate kira initialised the effect at (0 = not yet).
    mixer_rate: AtomicU32,
    /// The rate of the table the effect uses (0 = not yet).
    table_rate: AtomicU32,
}

impl Default for HrtfControl {
    fn default() -> Self {
        Self {
            seq: AtomicU32::new(0),
            x: AtomicU32::new(0.0f32.to_bits()),
            y: AtomicU32::new(0.0f32.to_bits()),
            z: AtomicU32::new((-1.0f32).to_bits()),
            blend: AtomicU32::new(1.0f32.to_bits()),
            mixer_rate: AtomicU32::new(0),
            table_rate: AtomicU32::new(0),
        }
    }
}

impl HrtfControl {
    /// The sample rate kira runs the effect at, once the effect is initialised.
    pub fn mixer_rate(&self) -> Option<u32> {
        Some(self.mixer_rate.load(Ordering::Relaxed)).filter(|r| *r != 0)
    }

    /// The rate of the HRIR table the effect convolves with, once initialised: the mixer rate,
    /// or the nearest prepared rate when the spectra had no table at the mixer rate.
    pub fn table_rate(&self) -> Option<u32> {
        Some(self.table_rate.load(Ordering::Relaxed)).filter(|r| *r != 0)
    }

    /// The mixer rate when the effect runs on a table of another rate (a better table can be
    /// built), else `None`.
    pub fn stale_rate(&self) -> Option<u32> {
        let mixer = self.mixer_rate()?;
        (self.table_rate() != Some(mixer)).then_some(mixer)
    }

    /// Records the rates (audio thread; two relaxed stores).
    fn note_rates(&self, mixer: u32, table: u32) {
        self.table_rate.store(table, Ordering::Relaxed);
        self.mixer_rate.store(mixer, Ordering::Relaxed);
    }

    /// Sets the emitter direction (head frame, any length; zero keeps "ahead") and the spatial
    /// blend (0 = dry/centred, 1 = fully binaural). One writer at a time (the game thread's
    /// sync); readers on other threads see either the old or the new values, never a mix.
    pub fn set(&self, direction: Vec3, blend: f32) {
        let dir = direction.normalize_or_zero();
        let dir = if dir == Vec3::ZERO || !dir.is_finite() {
            Vec3::NEG_Z
        } else {
            dir
        };
        let blend = if blend.is_finite() {
            blend.clamp(0.0, 1.0)
        } else {
            1.0
        };
        // Sequence lock: odd while storing, then even again with the values released.
        let seq = self.seq.load(Ordering::Relaxed);
        self.seq.store(seq.wrapping_add(1), Ordering::Relaxed);
        std::sync::atomic::fence(Ordering::Release);
        self.x.store(dir.x.to_bits(), Ordering::Relaxed);
        self.y.store(dir.y.to_bits(), Ordering::Relaxed);
        self.z.store(dir.z.to_bits(), Ordering::Relaxed);
        self.blend.store(blend.to_bits(), Ordering::Relaxed);
        self.seq.store(seq.wrapping_add(2), Ordering::Release);
    }

    /// The direction and blend last set. Lock- and wait-free for the audio thread: a read that
    /// keeps overlapping writes gives up after a few tries and returns the direction it read,
    /// renormalised (never a non-unit vector).
    pub fn get(&self) -> (Vec3, f32) {
        const TRIES: usize = 8;
        let f = |a: &AtomicU32| f32::from_bits(a.load(Ordering::Relaxed));
        let mut read = (Vec3::NEG_Z, 1.0);
        for _ in 0..TRIES {
            let before = self.seq.load(Ordering::Acquire);
            read = (Vec3::new(f(&self.x), f(&self.y), f(&self.z)), f(&self.blend));
            std::sync::atomic::fence(Ordering::Acquire);
            if before.is_multiple_of(2) && self.seq.load(Ordering::Relaxed) == before {
                return read;
            }
        }
        let dir = read.0.normalize_or_zero();
        (if dir == Vec3::ZERO { Vec3::NEG_Z } else { dir }, read.1)
    }
}

/// A validated HRIR sphere at [`HRIR_SAMPLE_RATE`]: unit source directions, the triangles
/// joining them (a closed surface around the head), and a left/right HRIR pair per direction.
#[derive(Clone, Debug, PartialEq)]
pub struct HrirSphere {
    points: Vec<Vec3>,
    faces: Vec<[usize; 3]>,
    left: Vec<Vec<f32>>,
    right: Vec<Vec<f32>>,
}

impl HrirSphere {
    /// The synthesized sphere: a level-2 icosphere (162 directions) with [`structural_hrir`]
    /// pairs of [`HRIR_LEN`] taps.
    pub fn synthesized() -> Self {
        let (vertices, faces) = icosphere(2);
        // Untilted: the icosphere is mirror-symmetric across the median plane and has vertices
        // exactly on the head axes (ahead, behind, each ear, up, down), so a source straight
        // ahead hears identical ears and a source at an ear gets that point's HRIR exactly.
        let points = vertices;
        Self {
            left: points
                .iter()
                .map(|p| structural_hrir(*p, Vec3::NEG_X, HRIR_SAMPLE_RATE))
                .collect(),
            right: points
                .iter()
                .map(|p| structural_hrir(*p, Vec3::X, HRIR_SAMPLE_RATE))
                .collect(),
            faces: faces
                .into_iter()
                .map(|f| [f[0] as usize, f[1] as usize, f[2] as usize])
                .collect(),
            points,
        }
    }

    /// Parses a `.hrir` file (the `hrtf` crate's format: `HRIR`, sample rate, HRIR length,
    /// point count, index count as little-endian `u32`, the triangle indices, then per point
    /// its position and left and right HRIRs as `f32`). The file is validated
    /// ([`validate_sphere_bytes`]); positions are projected onto the unit sphere and HRIRs at
    /// another rate are resampled to [`HRIR_SAMPLE_RATE`] (band-limited sinc,
    /// [`crate::resample_sinc`], scaled by `rate / HRIR_SAMPLE_RATE` so each HRIR keeps its DC
    /// gain, the tap sum).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, String> {
        validate_sphere_bytes(bytes)?;
        let u32_at = |o: usize| u32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]]);
        let f32_at = |o: usize| f32::from_bits(u32_at(o));
        let rate = u32_at(4);
        let length = u32_at(8) as usize;
        let vertex_count = u32_at(12) as usize;
        let index_count = u32_at(16) as usize;
        if rate == 0 {
            return Err("HRIR sphere with a zero sample rate".into());
        }
        let faces = (0..index_count / 3)
            .map(|f| {
                let at = 20 + f * 12;
                [
                    u32_at(at) as usize,
                    u32_at(at + 4) as usize,
                    u32_at(at + 8) as usize,
                ]
            })
            .collect();
        let faces_end = 20 + index_count * 4;
        let point_size = 12 + length * 8;
        let (mut points, mut left, mut right) = (Vec::new(), Vec::new(), Vec::new());
        for p in 0..vertex_count {
            let at = faces_end + p * point_size;
            points.push(Vec3::new(f32_at(at), f32_at(at + 4), f32_at(at + 8)).normalize());
            let read = |start: usize| -> Vec<f32> {
                let ir: Vec<f32> = (0..length).map(|i| f32_at(start + i * 4)).collect();
                // Band-limited, not linear: at an exact integer ratio (96 kHz file) linear
                // interpolation reads only ir[2n] and drops every odd tap — HRIRs are short
                // impulses whose leading delay carries the direction, so a 96 kHz sphere
                // would load with some directions silent and others +6 dB hot.
                resample_hrir(&ir, rate, HRIR_SAMPLE_RATE)
            };
            left.push(read(at + 12));
            right.push(read(at + 12 + length * 4));
        }
        if left.iter().chain(&right).flatten().any(|s| !s.is_finite()) {
            return Err("HRIR sphere has non-finite samples".into());
        }
        Ok(Self {
            points,
            faces,
            left,
            right,
        })
    }

    /// Serializes to the `.hrir` format at [`HRIR_SAMPLE_RATE`].
    pub fn to_bytes(&self) -> Vec<u8> {
        let len = self.hrir_len();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"HRIR");
        for v in [
            HRIR_SAMPLE_RATE,
            len as u32,
            self.points.len() as u32,
            (self.faces.len() * 3) as u32,
        ] {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        for face in &self.faces {
            for index in face {
                bytes.extend_from_slice(&(*index as u32).to_le_bytes());
            }
        }
        for (i, p) in self.points.iter().enumerate() {
            for c in [p.x, p.y, p.z] {
                bytes.extend_from_slice(&c.to_le_bytes());
            }
            for s in self.left[i].iter().chain(&self.right[i]) {
                bytes.extend_from_slice(&s.to_le_bytes());
            }
        }
        bytes
    }

    /// Rotates every direction (for a measured sphere recorded in another head frame).
    pub fn rotate(&mut self, rotation: Quat) {
        for p in &mut self.points {
            *p = (rotation * *p).normalize();
        }
    }

    /// The source directions (unit, head frame).
    pub fn points(&self) -> &[Vec3] {
        &self.points
    }

    /// The left- and right-ear HRIRs of point `index`.
    pub fn hrirs(&self, index: usize) -> Option<(&[f32], &[f32])> {
        Some((self.left.get(index)?, self.right.get(index)?))
    }

    /// Taps per HRIR.
    pub fn hrir_len(&self) -> usize {
        self.left.first().map_or(0, Vec::len)
    }

    /// The frequency-domain tables every binaural emitter shares: this sphere at
    /// [`HRIR_SAMPLE_RATE`] and resampled to [`HRIR_ALT_SAMPLE_RATE`]. Tables at other rates are
    /// added with [`SphereSpectra::with_rate`].
    pub fn spectra(&self) -> Arc<SphereSpectra> {
        Arc::new(SphereSpectra::new(self))
    }

    /// The HRIRs resampled from [`HRIR_SAMPLE_RATE`] to `rate` (band-limited sinc,
    /// [`crate::resample_sinc`], scaled to keep each HRIR's DC gain) and cut or zero-padded to
    /// the same tap count, so both tables share one convolution size. Tables above
    /// [`HRIR_SAMPLE_RATE`] keep every resampled tap instead ([`SphereSpectra::with_rate`]); this
    /// cut form is the [`HRIR_ALT_SAMPLE_RATE`] table's.
    #[must_use]
    pub fn resampled_to(&self, rate: u32) -> Self {
        self.resampled_with_taps(rate, self.hrir_len())
    }

    /// The HRIRs resampled to `rate` and cut or zero-padded to `taps`.
    fn resampled_with_taps(&self, rate: u32, taps: usize) -> Self {
        let fit = |ir: &Vec<f32>| -> Vec<f32> {
            let mut out = resample_hrir(ir, HRIR_SAMPLE_RATE, rate);
            out.resize(taps, 0.0);
            out
        };
        Self {
            points: self.points.clone(),
            faces: self.faces.clone(),
            left: self.left.iter().map(fit).collect(),
            right: self.right.iter().map(fit).collect(),
        }
    }
}

/// Resamples an impulse response from `from` to `to` Hz and keeps its DC gain. `resample_sinc`
/// keeps a signal's amplitude, so an impulse response's tap sum (its gain at 0 Hz) scales with
/// the tap count, `to / from`; the `from / to` factor undoes that (as the old linear loader
/// did), so a sphere converted from a 96 or 44.1 kHz set plays at the level it was measured at.
fn resample_hrir(ir: &[f32], from: u32, to: u32) -> Vec<f32> {
    let mut out = crate::resample_sinc(ir, from, to);
    if from != to && from != 0 && to != 0 {
        let gain = (f64::from(from) / f64::from(to)) as f32;
        for sample in &mut out {
            *sample *= gain;
        }
    }
    out
}

/// One rate's HRIR spectra, with that rate's convolution size and FFT plans.
struct RateTable {
    rate: u32,
    /// Taps per HRIR at this rate (the overlap-save history is one less).
    hrir_len: usize,
    /// FFT size: [`HRTF_BLOCK_LEN`] `+ hrir_len − 1`.
    pad_len: usize,
    fft: Arc<dyn Fft<f32>>,
    ifft: Arc<dyn Fft<f32>>,
    left: Vec<Vec<Complex<f32>>>,
    right: Vec<Vec<Complex<f32>>>,
}

impl RateTable {
    /// The table of `sphere` (at [`HRIR_SAMPLE_RATE`]) at `rate`. At or below
    /// [`HRIR_SAMPLE_RATE`] the HRIRs keep the sphere's tap count (the [`HRIR_ALT_SAMPLE_RATE`]
    /// table is built this way, as before ADR-7461); above it they keep every resampled tap, so
    /// the interaural delay and the pinna echoes keep their length in time, and are zero-padded
    /// so the FFT size is a multiple of [`HRTF_BLOCK_LEN`] (a smooth size, not a prime).
    fn build(sphere: &HrirSphere, rate: u32, planner: &mut FftPlanner<f32>) -> Self {
        let len = sphere.hrir_len().max(1);
        let hrir_len = if rate <= HRIR_SAMPLE_RATE {
            len
        } else {
            let resampled = (len as u64 * u64::from(rate)).div_ceil(u64::from(HRIR_SAMPLE_RATE)) as usize;
            (resampled + HRTF_BLOCK_LEN - 1).next_multiple_of(HRTF_BLOCK_LEN) - (HRTF_BLOCK_LEN - 1)
        };
        let pad_len = HRTF_BLOCK_LEN + hrir_len - 1;
        let fft = planner.plan_fft_forward(pad_len);
        let ifft = planner.plan_fft_inverse(pad_len);
        let spectrum = |ir: &[f32]| -> Vec<Complex<f32>> {
            let mut buf: Vec<Complex<f32>> = (0..pad_len)
                .map(|i| Complex::new(ir.get(i).copied().unwrap_or(0.0), 0.0))
                .collect();
            fft.process(&mut buf);
            buf
        };
        let resampled;
        let source = if rate == HRIR_SAMPLE_RATE {
            sphere
        } else {
            resampled = sphere.resampled_with_taps(rate, hrir_len);
            &resampled
        };
        Self {
            rate,
            hrir_len,
            pad_len,
            left: source.left.iter().map(|ir| spectrum(ir)).collect(),
            right: source.right.iter().map(|ir| spectrum(ir)).collect(),
            fft,
            ifft,
        }
    }

    /// Spectrum bytes of this table.
    fn bytes(&self) -> usize {
        (self.left.len() + self.right.len()) * self.pad_len * std::mem::size_of::<Complex<f32>>()
    }
}

/// The HRIR spectra of a sphere (zero-padded to each rate's convolution size), shared by every
/// binaural emitter: tables at [`HRIR_SAMPLE_RATE`] and [`HRIR_ALT_SAMPLE_RATE`] from the start,
/// and at most [`HRTF_MAX_RATE_TABLES`] in all once [`Self::with_rate`] has added others.
pub struct SphereSpectra {
    points: Vec<Vec3>,
    faces: Vec<[usize; 3]>,
    tables: Vec<Arc<RateTable>>,
    /// The sphere at [`HRIR_SAMPLE_RATE`], kept to build tables at other rates.
    sphere: Arc<HrirSphere>,
    /// The largest FFT size, history and FFT scratch of any table (convolver buffer sizes).
    max_pad: usize,
    max_history: usize,
    max_scratch: usize,
}

impl SphereSpectra {
    fn new(sphere: &HrirSphere) -> Self {
        let mut planner = FftPlanner::new();
        let tables = [HRIR_SAMPLE_RATE, HRIR_ALT_SAMPLE_RATE]
            .into_iter()
            .map(|rate| Arc::new(RateTable::build(sphere, rate, &mut planner)))
            .collect();
        Self::from_tables(
            Arc::new(sphere.clone()),
            sphere.points.clone(),
            sphere.faces.clone(),
            tables,
        )
    }

    fn from_tables(
        sphere: Arc<HrirSphere>,
        points: Vec<Vec3>,
        faces: Vec<[usize; 3]>,
        tables: Vec<Arc<RateTable>>,
    ) -> Self {
        let max_pad = tables.iter().map(|t| t.pad_len).max().unwrap_or(1);
        let max_history = tables.iter().map(|t| t.hrir_len - 1).max().unwrap_or(0);
        let max_scratch = tables
            .iter()
            .map(|t| {
                t.fft
                    .get_inplace_scratch_len()
                    .max(t.ifft.get_inplace_scratch_len())
            })
            .max()
            .unwrap_or(0);
        Self {
            points,
            faces,
            tables,
            sphere,
            max_pad,
            max_history,
            max_scratch,
        }
    }

    /// These spectra with a table at `rate` added (game thread: resamples the sphere and
    /// transforms every HRIR, about a millisecond for the synthesized sphere). The tables
    /// already built are shared, not copied; effects built over `self` keep using `self`.
    /// Returns `self` unchanged when it already has a table at `rate`, when `rate` is 0, or
    /// when it holds [`HRTF_MAX_RATE_TABLES`] tables (the mixer then uses the nearest one).
    #[must_use]
    pub fn with_rate(self: &Arc<Self>, rate: u32) -> Arc<Self> {
        if rate == 0 || self.has_rate(rate) || self.tables.len() >= HRTF_MAX_RATE_TABLES {
            return Arc::clone(self);
        }
        let _span = tracing::info_span!("hrtf_rate_table", rate).entered();
        let mut planner = FftPlanner::new();
        let mut tables = self.tables.clone();
        tables.push(Arc::new(RateTable::build(&self.sphere, rate, &mut planner)));
        Arc::new(Self::from_tables(
            Arc::clone(&self.sphere),
            self.points.clone(),
            self.faces.clone(),
            tables,
        ))
    }

    /// True when a table at exactly `rate` exists.
    #[must_use]
    pub fn has_rate(&self, rate: u32) -> bool {
        self.tables.iter().any(|t| t.rate == rate)
    }

    /// The rate of the table a mixer at `rate` uses: `rate` itself when it has a table, else
    /// the nearest prepared rate.
    #[must_use]
    pub fn table_rate_for(&self, rate: u32) -> u32 {
        self.tables[self.table_for(rate)].rate
    }

    /// Bytes the spectra hold (every table's left and right HRIR spectra).
    #[must_use]
    pub fn table_bytes(&self) -> usize {
        self.tables.iter().map(|t| t.bytes()).sum()
    }

    /// The index of the table at `rate`, else the nearest one.
    fn table_for(&self, rate: u32) -> usize {
        self.tables
            .iter()
            .enumerate()
            .min_by_key(|(_, t)| t.rate.abs_diff(rate))
            .map_or(0, |(i, _)| i)
    }

    /// The rates the spectra are prepared at, in the order the tables were built.
    #[must_use]
    pub fn rates(&self) -> Vec<u32> {
        self.tables.iter().map(|t| t.rate).collect()
    }

    /// The face the ray from the head centre along `dir` passes through, with the barycentric
    /// weights of its three points. Tries `hint` first (directions move slowly).
    fn locate(&self, dir: Vec3, hint: usize) -> Option<(usize, [f32; 3])> {
        if let Some(weights) = self.faces.get(hint).and_then(|f| self.weights(f, dir)) {
            return Some((hint, weights));
        }
        self.faces
            .iter()
            .enumerate()
            .find_map(|(i, f)| self.weights(f, dir).map(|w| (i, w)))
    }

    /// Barycentric weights of the hit of the ray `t·dir` (`t > 0`) on face `f`, if it hits.
    fn weights(&self, f: &[usize; 3], dir: Vec3) -> Option<[f32; 3]> {
        let (a, b, c) = (self.points[f[0]], self.points[f[1]], self.points[f[2]]);
        let normal = (b - a).cross(c - a);
        let along = dir.dot(normal);
        if along.abs() < 1e-12 {
            return None;
        }
        let t = a.dot(normal) / along;
        if t <= 0.0 {
            return None;
        }
        let p = dir * t;
        let (v0, v1, v2) = (b - a, c - a, p - a);
        let (d00, d01, d11) = (v0.dot(v0), v0.dot(v1), v1.dot(v1));
        let (d20, d21) = (v2.dot(v0), v2.dot(v1));
        let denom = d00 * d11 - d01 * d01;
        if denom.abs() < 1e-12 {
            return None;
        }
        let v = (d11 * d20 - d01 * d21) / denom;
        let w = (d00 * d21 - d01 * d20) / denom;
        let u = 1.0 - v - w;
        const EPS: f32 = 1e-4;
        (u >= -EPS && v >= -EPS && w >= -EPS).then_some([u, v, w])
    }
}

/// One emitter's convolution state over a shared [`SphereSpectra`]. Its buffers are sized for
/// the largest table of its spectra when it is built (on the game thread), so switching to
/// another table on the audio thread allocates nothing.
struct Convolver {
    sphere: Arc<SphereSpectra>,
    /// FFT work buffer (input history + one sub-block); the table's `pad_len` is used.
    input: Vec<Complex<f32>>,
    ear: Vec<Complex<f32>>,
    scratch: Vec<Complex<f32>>,
    /// The interpolated spectra of the current direction.
    left: Vec<Complex<f32>>,
    right: Vec<Complex<f32>>,
    /// The last `hrir_len − 1` input samples (overlap-save history) of the table in use.
    history: Vec<f32>,
    face_hint: usize,
    /// The rate table in use ([`SphereSpectra::tables`] index).
    table: usize,
}

impl Convolver {
    fn new(sphere: Arc<SphereSpectra>) -> Self {
        let pad = sphere.max_pad;
        Self {
            input: vec![Complex::new(0.0, 0.0); pad],
            ear: vec![Complex::new(0.0, 0.0); pad],
            scratch: vec![Complex::new(0.0, 0.0); sphere.max_scratch],
            left: vec![Complex::new(0.0, 0.0); pad],
            right: vec![Complex::new(0.0, 0.0); pad],
            history: vec![0.0; sphere.max_history],
            face_hint: 0,
            table: 0,
            sphere,
        }
    }

    /// Uses the table at `rate` (the rate kira runs the effect at), else the nearest one.
    /// Allocation-free; a change of table clears the convolution history (its length is the
    /// table's). Returns the rate of the table in use.
    fn select_rate(&mut self, rate: u32) -> u32 {
        let table = self.sphere.table_for(rate);
        if table != self.table {
            self.table = table;
            self.history.fill(0.0);
        }
        self.sphere.tables[self.table].rate
    }

    /// Convolves one chunk of mono `source` into `out`, moving the direction from `from` to
    /// `to` across the [`HRTF_INTERPOLATION_STEPS`] sub-blocks.
    fn process(&mut self, source: &[f32], out: &mut [(f32, f32)], from: Vec3, to: Vec3) {
        let Self {
            sphere,
            input,
            ear: ear_buf,
            scratch,
            left,
            right,
            history,
            face_hint,
            table,
        } = self;
        let t: &RateTable = &sphere.tables[*table];
        let pad = t.pad_len;
        let hist = t.hrir_len - 1;
        let (input, ear_buf, left, right, history) = (
            &mut input[..pad],
            &mut ear_buf[..pad],
            &mut left[..pad],
            &mut right[..pad],
            &mut history[..hist],
        );
        let norm = 1.0 / pad as f32;
        for step in 0..HRTF_INTERPOLATION_STEPS {
            let s = (step + 1) as f32 / HRTF_INTERPOLATION_STEPS as f32;
            let dir = from.lerp(to, s).normalize_or_zero();
            steer(
                sphere,
                t,
                face_hint,
                left,
                right,
                if dir == Vec3::ZERO { to } else { dir },
            );
            let block = &source[step * HRTF_BLOCK_LEN..(step + 1) * HRTF_BLOCK_LEN];
            // Overlap-save: [history | new block], then keep the newest `hist` samples.
            for (slot, s) in input.iter_mut().zip(history.iter().chain(block)) {
                *slot = Complex::new(*s, 0.0);
            }
            for (h, slot) in history.iter_mut().zip(&input[pad - hist..]) {
                *h = slot.re;
            }
            t.fft.process_with_scratch(input, scratch);
            let outs = &mut out[step * HRTF_BLOCK_LEN..(step + 1) * HRTF_BLOCK_LEN];
            for ear in 0..2 {
                let h: &[Complex<f32>] = if ear == 0 { left } else { right };
                for ((e, x), h) in ear_buf.iter_mut().zip(input.iter()).zip(h) {
                    *e = *x * *h;
                }
                t.ifft.process_with_scratch(ear_buf, scratch);
                for (o, y) in outs.iter_mut().zip(&ear_buf[hist..]) {
                    if ear == 0 {
                        o.0 = y.re * norm;
                    } else {
                        o.1 = y.re * norm;
                    }
                }
            }
        }
    }
}

/// Interpolates table `t`'s spectra for `dir` into `left`/`right` (`t.pad_len` bins each);
/// keeps the previous ones if no face is hit.
fn steer(
    sphere: &SphereSpectra,
    t: &RateTable,
    face_hint: &mut usize,
    left: &mut [Complex<f32>],
    right: &mut [Complex<f32>],
    dir: Vec3,
) {
    let Some((face, w)) = sphere.locate(dir, *face_hint) else {
        return;
    };
    *face_hint = face;
    let f = sphere.faces[face];
    for i in 0..t.pad_len {
        left[i] = t.left[f[0]][i] * w[0] + t.left[f[1]][i] * w[1] + t.left[f[2]][i] * w[2];
        right[i] = t.right[f[0]][i] * w[0] + t.right[f[1]][i] * w[1] + t.right[f[2]][i] * w[2];
    }
}

/// Builds the binaural effect for one emitter track.
pub struct HrtfEffectBuilder {
    convolver: Convolver,
    control: Arc<HrtfControl>,
}

impl HrtfEffectBuilder {
    /// A binaural effect over the shared `spectra`, steered through `control`. The effect
    /// convolves with the table at the rate kira initialises it at, or the nearest table when
    /// `spectra` has none at that rate; it reports both rates through `control`
    /// ([`HrtfControl::stale_rate`]), and the audio system then adds the missing table
    /// ([`SphereSpectra::with_rate`]) and rebuilds the track (ADR-7461).
    pub fn new(spectra: Arc<SphereSpectra>, control: Arc<HrtfControl>) -> Self {
        Self {
            convolver: Convolver::new(spectra),
            control,
        }
    }
}

impl EffectBuilder for HrtfEffectBuilder {
    type Handle = ();

    fn build(self) -> (Box<dyn Effect>, Self::Handle) {
        let (direction, blend) = self.control.get();
        (
            Box::new(HrtfEffect {
                convolver: self.convolver,
                control: self.control,
                input: Vec::with_capacity(CHUNK),
                dry: Vec::with_capacity(CHUNK),
                output: vec![(0.0, 0.0); CHUNK],
                // One chunk of latency: the first chunk is emitted while the next one fills.
                pending: vec![Frame::ZERO; CHUNK],
                pending_read: 0,
                prev_direction: direction,
                prev_blend: blend,
            }),
            (),
        )
    }
}

/// The kira effect: folds the track to mono, convolves it with the interpolated HRIR pair and
/// crossfades with the dry signal by the spatial blend. Allocation-free after construction.
struct HrtfEffect {
    convolver: Convolver,
    control: Arc<HrtfControl>,
    /// Mono input of the chunk being filled.
    input: Vec<f32>,
    /// Dry stereo of the chunk being filled (for the blend).
    dry: Vec<Frame>,
    /// Convolver output scratch.
    output: Vec<(f32, f32)>,
    /// The previous chunk's output, being drained into kira's buffers.
    pending: Vec<Frame>,
    pending_read: usize,
    prev_direction: Vec3,
    prev_blend: f32,
}

impl HrtfEffect {
    /// Convolves the filled chunk into `pending`.
    fn process_chunk(&mut self) {
        let (direction, blend) = self.control.get();
        self.convolver
            .process(&self.input, &mut self.output, self.prev_direction, direction);
        for (i, frame) in self.pending.iter_mut().enumerate() {
            let t = (i + 1) as f32 / CHUNK as f32;
            let b = self.prev_blend + (blend - self.prev_blend) * t;
            let dry = self.dry[i];
            let (wl, wr) = self.output[i];
            *frame = Frame {
                left: dry.left * (1.0 - b) + wl * b,
                right: dry.right * (1.0 - b) + wr * b,
            };
        }
        self.prev_direction = direction;
        self.prev_blend = blend;
        self.pending_read = 0;
        self.input.clear();
        self.dry.clear();
    }
}

impl Effect for HrtfEffect {
    fn init(&mut self, sample_rate: u32, _internal_buffer_size: usize) {
        let table = self.convolver.select_rate(sample_rate);
        self.control.note_rates(sample_rate, table);
    }

    fn on_change_sample_rate(&mut self, sample_rate: u32) {
        let table = self.convolver.select_rate(sample_rate);
        self.control.note_rates(sample_rate, table);
    }

    fn process(&mut self, input: &mut [Frame], _dt: f64, _info: &Info) {
        for frame in input.iter_mut() {
            self.input.push((frame.left + frame.right) * 0.5);
            self.dry.push(*frame);
            *frame = self.pending[self.pending_read];
            self.pending_read += 1;
            if self.input.len() == CHUNK {
                self.process_chunk();
            }
        }
    }
}

/// Builds an icosphere: the 12 icosahedron vertices subdivided `levels` times, projected onto
/// the unit sphere. Returns the vertices and the triangles.
fn icosphere(levels: u32) -> (Vec<Vec3>, Vec<[u32; 3]>) {
    let t = (1.0 + 5.0f32.sqrt()) / 2.0;
    let mut vertices: Vec<Vec3> = [
        (-1.0, t, 0.0),
        (1.0, t, 0.0),
        (-1.0, -t, 0.0),
        (1.0, -t, 0.0),
        (0.0, -1.0, t),
        (0.0, 1.0, t),
        (0.0, -1.0, -t),
        (0.0, 1.0, -t),
        (t, 0.0, -1.0),
        (t, 0.0, 1.0),
        (-t, 0.0, -1.0),
        (-t, 0.0, 1.0),
    ]
    .iter()
    .map(|&(x, y, z)| Vec3::new(x, y, z).normalize())
    .collect();
    let mut faces: Vec<[u32; 3]> = vec![
        [0, 11, 5],
        [0, 5, 1],
        [0, 1, 7],
        [0, 7, 10],
        [0, 10, 11],
        [1, 5, 9],
        [5, 11, 4],
        [11, 10, 2],
        [10, 7, 6],
        [7, 1, 8],
        [3, 9, 4],
        [3, 4, 2],
        [3, 2, 6],
        [3, 6, 8],
        [3, 8, 9],
        [4, 9, 5],
        [2, 4, 11],
        [6, 2, 10],
        [8, 6, 7],
        [9, 8, 1],
    ];
    for _ in 0..levels {
        let mut midpoints = std::collections::HashMap::<(u32, u32), u32>::new();
        let mut midpoint = |a: u32, b: u32, vertices: &mut Vec<Vec3>| -> u32 {
            let key = (a.min(b), a.max(b));
            *midpoints.entry(key).or_insert_with(|| {
                let m = ((vertices[a as usize] + vertices[b as usize]) * 0.5).normalize();
                vertices.push(m);
                (vertices.len() - 1) as u32
            })
        };
        let mut next = Vec::with_capacity(faces.len() * 4);
        for [a, b, c] in faces {
            let ab = midpoint(a, b, &mut vertices);
            let bc = midpoint(b, c, &mut vertices);
            let ca = midpoint(c, a, &mut vertices);
            next.extend([[a, ab, ca], [b, bc, ab], [c, ca, bc], [ab, bc, ca]]);
        }
        faces = next;
    }
    (vertices, faces)
}

/// Windowed-sinc (Hann) fractional delay: adds `gain · δ(n − delay)` into `out`.
fn add_impulse(out: &mut [f32], delay: f32, gain: f32) {
    let centre = delay.floor() as isize;
    let half = SINC_HALF as isize;
    for n in (centre - half + 1)..=(centre + half) {
        if n < 0 || n as usize >= out.len() {
            continue;
        }
        let x = n as f32 - delay;
        let sinc = if x.abs() < 1e-6 {
            1.0
        } else {
            (std::f32::consts::PI * x).sin() / (std::f32::consts::PI * x)
        };
        let window = 0.5 + 0.5 * (std::f32::consts::PI * x / SINC_HALF as f32).cos();
        if x.abs() < SINC_HALF as f32 {
            out[n as usize] += gain * sinc * window;
        }
    }
}

/// One ear's impulse response for a source in head-frame direction `dir` (unit), with
/// `ear_axis` = ±X (Brown & Duda's structural model, see the module docs).
pub fn structural_hrir(dir: Vec3, ear_axis: Vec3, sample_rate: u32) -> Vec<f32> {
    let fs = sample_rate as f32;
    // Angle of incidence at this ear: 0 = straight at it, π = the far side.
    let theta = dir.dot(ear_axis).clamp(-1.0, 1.0).acos();
    // Interaural delay (eq. 4), offset so the near side is 0.
    let head = HEAD_RADIUS / SOUND_SPEED;
    let itd = if theta < std::f32::consts::FRAC_PI_2 {
        -head * theta.cos()
    } else {
        head * (theta - std::f32::consts::FRAC_PI_2)
    } + head;
    let base = SINC_HALF as f32 + itd * fs;

    // Pinna: direct path plus five echoes (eq. 5). Azimuth here is the lateral angle toward
    // this ear; elevation runs over the median plane from ahead (0) over the top (π/2) to
    // behind (π), so front and back differ.
    let mut ir = vec![0.0f32; HRIR_LEN];
    let lateral = dir.dot(ear_axis).clamp(-1.0, 1.0).asin();
    let elevation = dir.y.atan2(-dir.z);
    add_impulse(&mut ir, base, 1.0);
    for (rho, a, b, d) in PINNA {
        let tau = a * (lateral / 2.0).cos() * (d * (std::f32::consts::FRAC_PI_2 - elevation)).sin() + b;
        add_impulse(&mut ir, base + tau.max(0.0) * fs / 44_100.0, rho);
    }

    // Head shadow (eqs. 2–3): H(s) = (1 + α s / 2ω₀) / (1 + s / 2ω₀), ω₀ = c / a, discretised
    // with the bilinear transform.
    let alpha = (1.0 + ALPHA_MIN / 2.0)
        + (1.0 - ALPHA_MIN / 2.0) * ((theta / THETA_MIN).min(1.0) * std::f32::consts::PI).cos();
    let beta = 2.0 * SOUND_SPEED / HEAD_RADIUS;
    let k = 2.0 * fs;
    let b0 = (beta + alpha * k) / (beta + k);
    let b1 = (beta - alpha * k) / (beta + k);
    let a1 = (beta - k) / (beta + k);
    // Unity DC gain ahead: binaural voices play centred, so the folded input already carries
    // kira's −3 dB centre-pan level and the result matches a panned voice's loudness.
    let (mut x1, mut y1) = (0.0f32, 0.0f32);
    for sample in &mut ir {
        let x = *sample;
        let y = b0 * x + b1 * x1 - a1 * y1;
        x1 = x;
        y1 = y;
        *sample = y;
    }
    ir
}

/// Structural checks on a `.hrir` file: header, face indices in range, vertex positions finite
/// and away from the centre, and a closed surface (every edge shared by
/// exactly two faces) so every direction lands on a face.
pub fn validate_sphere_bytes(bytes: &[u8]) -> Result<(), String> {
    let u32_at = |offset: usize| -> Result<u32, String> {
        bytes
            .get(offset..offset + 4)
            .and_then(|b| b.try_into().ok())
            .map(u32::from_le_bytes)
            .ok_or_else(|| "truncated HRIR sphere".to_string())
    };
    if bytes.get(0..4) != Some(b"HRIR".as_slice()) {
        return Err("not an HRIR sphere (bad magic)".into());
    }
    let length = u32_at(8)? as usize;
    let vertex_count = u32_at(12)? as usize;
    let index_count = u32_at(16)? as usize;
    if length == 0 || vertex_count < 4 || index_count < 12 || !index_count.is_multiple_of(3) {
        return Err(format!(
            "degenerate HRIR sphere: {vertex_count} points, {index_count} indices, {length} taps"
        ));
    }
    let faces_end = 20 + index_count * 4;
    let point_size = 12 + length * 8;
    if bytes.len() < faces_end + vertex_count * point_size {
        return Err("truncated HRIR sphere".into());
    }
    let mut edges = std::collections::HashMap::<(u32, u32), u32>::new();
    for face in 0..index_count / 3 {
        let mut idx = [0u32; 3];
        for (k, slot) in idx.iter_mut().enumerate() {
            *slot = u32_at(20 + (face * 3 + k) * 4)?;
            if *slot as usize >= vertex_count {
                return Err(format!("face {face} indexes point {slot} of {vertex_count}"));
            }
        }
        for (a, b) in [(idx[0], idx[1]), (idx[1], idx[2]), (idx[2], idx[0])] {
            *edges.entry((a.min(b), a.max(b))).or_insert(0) += 1;
        }
    }
    if let Some(((a, b), n)) = edges.iter().find(|(_, n)| **n != 2) {
        return Err(format!(
            "HRIR sphere is not closed: edge {a}–{b} is shared by {n} faces"
        ));
    }
    for point in 0..vertex_count {
        let at = faces_end + point * point_size;
        let mut p = [0.0f32; 3];
        for (k, c) in p.iter_mut().enumerate() {
            *c = f32::from_bits(u32_at(at + k * 4)?);
        }
        let len = Vec3::from(p).length();
        if !len.is_finite() || !(0.05..9.0).contains(&len) {
            return Err(format!("HRIR point {point} lies at radius {len}"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The game thread sweeps an emitter between two directions while the audio thread reads:
    /// a read must never mix the components of two directions (a vector that is not even unit
    /// length, steering one chunk somewhere unset; concurrency audit 11-03 L5). Counted
    /// iterations, no timing.
    #[test]
    fn a_read_never_mixes_two_directions() {
        let control = Arc::new(HrtfControl::default());
        let a = (Vec3::X, 0.25f32);
        let b = (Vec3::Y, 0.75f32);
        control.set(a.0, a.1);
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let writer = {
            let control = Arc::clone(&control);
            let done = Arc::clone(&done);
            std::thread::spawn(move || {
                for i in 0..2_000_000u32 {
                    let (d, blend) = if i.is_multiple_of(2) { b } else { a };
                    control.set(d, blend);
                }
                done.store(true, Ordering::Release);
            })
        };
        let mut torn = 0u64;
        let mut reads = 0u64;
        while !done.load(Ordering::Acquire) || reads < 1000 {
            let got = control.get();
            reads += 1;
            if got != a && got != b && (got.0.length() - 1.0).abs() > 1e-5 {
                torn += 1;
            }
        }
        writer.join().expect("writer");
        assert_eq!(torn, 0, "{torn} of {reads} reads mixed two sets");
    }

    fn energy(ir: &[f32]) -> f32 {
        ir.iter().map(|s| s * s).sum()
    }

    /// Index of the strongest tap (the arrival time).
    fn arrival(ir: &[f32]) -> usize {
        ir.iter()
            .enumerate()
            .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
            .map(|(i, _)| i)
            .unwrap_or(0)
    }

    #[test]
    fn icosphere_level_two_is_closed() {
        let (vertices, faces) = icosphere(2);
        assert_eq!(vertices.len(), 162);
        assert_eq!(faces.len(), 320);
        // Euler: V − E + F = 2 with E = 3F/2.
        assert_eq!(
            vertices.len() as i64 - (faces.len() * 3 / 2) as i64 + faces.len() as i64,
            2
        );
        assert!(vertices.iter().all(|v| (v.length() - 1.0).abs() < 1e-5));
    }

    #[test]
    fn a_source_on_the_right_reaches_the_right_ear_first_and_louder() {
        let right = structural_hrir(Vec3::X, Vec3::X, HRIR_SAMPLE_RATE);
        let left = structural_hrir(Vec3::X, Vec3::NEG_X, HRIR_SAMPLE_RATE);
        // Interaural time difference ≈ (a/c)(1 + π/2) ≈ 0.66 ms ≈ 31 samples.
        let itd = arrival(&left) as i64 - arrival(&right) as i64;
        assert!((25..=37).contains(&itd), "ITD {itd} samples");
        // Head shadow: the far ear loses high frequencies → less energy.
        assert!(
            energy(&right) > 1.5 * energy(&left),
            "{} vs {}",
            energy(&right),
            energy(&left)
        );
    }

    #[test]
    fn head_shadow_holds_its_floor_beyond_the_minimum_angle() {
        // A source due left seen from the right ear (theta = 180 degrees) must be shadowed at
        // least as much as at 150 degrees, never brighter.
        let at_150 = Vec3::new(150f32.to_radians().cos(), 0.0, -150f32.to_radians().sin());
        let far = structural_hrir(Vec3::NEG_X, Vec3::X, HRIR_SAMPLE_RATE);
        let near_floor = structural_hrir(at_150, Vec3::X, HRIR_SAMPLE_RATE);
        assert!(
            energy(&far) <= energy(&near_floor) * 1.05,
            "{} vs {}",
            energy(&far),
            energy(&near_floor)
        );
    }

    #[test]
    fn a_frontal_source_is_symmetric_and_front_differs_from_back() {
        let l = structural_hrir(Vec3::NEG_Z, Vec3::NEG_X, HRIR_SAMPLE_RATE);
        let r = structural_hrir(Vec3::NEG_Z, Vec3::X, HRIR_SAMPLE_RATE);
        for (a, b) in l.iter().zip(&r) {
            assert!((a - b).abs() < 1e-6);
        }
        let back = structural_hrir(Vec3::Z, Vec3::X, HRIR_SAMPLE_RATE);
        let diff: f32 = r.iter().zip(&back).map(|(a, b)| (a - b).abs()).sum();
        assert!(diff > 0.05, "front/back cues identical ({diff})");
    }

    /// Convolves `chunks` chunks of deterministic noise from `dir`; returns (left, right).
    fn convolve_noise(sphere: &HrirSphere, dir: Vec3, chunks: usize) -> (Vec<f32>, Vec<f32>) {
        let mut convolver = Convolver::new(sphere.spectra());
        let mut state = 7u32;
        let (mut left, mut right) = (Vec::new(), Vec::new());
        for _ in 0..chunks {
            let source: Vec<f32> = (0..CHUNK)
                .map(|_| {
                    state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    (state >> 8) as f32 / (1u32 << 24) as f32 - 0.5
                })
                .collect();
            let mut out = vec![(0.0f32, 0.0f32); CHUNK];
            convolver.process(&source, &mut out, dir, dir);
            left.extend(out.iter().map(|o| o.0));
            right.extend(out.iter().map(|o| o.1));
        }
        (left, right)
    }

    /// Lag (samples) maximizing the cross-correlation; positive when the left ear is later.
    fn lag(left: &[f32], right: &[f32]) -> i64 {
        let mut best = (0i64, f32::MIN);
        for lag in -60i64..=60 {
            let sum: f32 = (60..left.len() - 60)
                .map(|i| right[i] * left[(i as i64 + lag) as usize])
                .sum();
            if sum > best.1 {
                best = (lag, sum);
            }
        }
        best.0
    }

    #[test]
    fn the_convolver_places_directions_correctly() {
        let sphere = HrirSphere::synthesized();
        let (l, r) = convolve_noise(&sphere, Vec3::X, 16);
        let lag_right = lag(&l, &r);
        assert!((25..=37).contains(&lag_right), "right source: lag {lag_right}");
        assert!(
            energy(&r) > 1.5 * energy(&l),
            "right source: {} vs {}",
            energy(&r),
            energy(&l)
        );
        let (l, r) = convolve_noise(&sphere, Vec3::NEG_X, 16);
        let lag_left = lag(&l, &r);
        assert!((-37..=-25).contains(&lag_left), "left source: lag {lag_left}");
        assert!(energy(&l) > 1.5 * energy(&r));
        let (l, r) = convolve_noise(&sphere, Vec3::NEG_Z, 16);
        assert!(lag(&l, &r).abs() <= 1, "ahead");
        assert!((energy(&l) / energy(&r) - 1.0).abs() < 0.2, "ahead is balanced");
    }

    #[test]
    fn convolution_matches_direct_convolution_at_a_sphere_point() {
        // At a sphere point the interpolated response is that point's HRIR exactly; the
        // overlap-save output must equal the direct convolution.
        let sphere = HrirSphere::synthesized();
        let point = sphere.points()[40];
        let (hl, _) = sphere.hrirs(40).expect("point");
        let mut convolver = Convolver::new(sphere.spectra());
        let source: Vec<f32> = (0..CHUNK * 3)
            .map(|i| ((i * 37 % 101) as f32 / 50.0) - 1.0)
            .collect();
        let mut got = Vec::new();
        for chunk in source.chunks(CHUNK) {
            let mut out = vec![(0.0f32, 0.0f32); CHUNK];
            convolver.process(chunk, &mut out, point, point);
            got.extend(out.iter().map(|o| o.0));
        }
        for n in 0..source.len() {
            let direct: f32 = (0..hl.len().min(n + 1)).map(|k| hl[k] * source[n - k]).sum();
            assert!(
                (got[n] - direct).abs() < 1e-3,
                "sample {n}: {} vs {direct}",
                got[n]
            );
        }
    }

    /// Gap audit B (ADR-2721): on a 44.1 kHz mixer the interaural delay is the model's, not 9 %
    /// long. ITD = (a/c)(1 + π/2) = 0.656 ms: 28.9 samples at 44.1 kHz, 31.5 at 48 kHz.
    #[test]
    fn a_44k_mixer_uses_the_44k_table() {
        let sphere = HrirSphere::synthesized();
        let spectra = sphere.spectra();
        assert_eq!(spectra.rates(), vec![HRIR_SAMPLE_RATE, HRIR_ALT_SAMPLE_RATE]);
        let lag_at = |rate: u32| {
            let mut convolver = Convolver::new(Arc::clone(&spectra));
            convolver.select_rate(rate);
            let mut state = 7u32;
            let (mut left, mut right) = (Vec::new(), Vec::new());
            for _ in 0..16 {
                let source: Vec<f32> = (0..CHUNK)
                    .map(|_| {
                        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                        (state >> 8) as f32 / (1u32 << 24) as f32 - 0.5
                    })
                    .collect();
                let mut out = vec![(0.0f32, 0.0f32); CHUNK];
                convolver.process(&source, &mut out, Vec3::X, Vec3::X);
                left.extend(out.iter().map(|o| o.0));
                right.extend(out.iter().map(|o| o.1));
            }
            lag(&left, &right)
        };
        let at_44 = lag_at(44_100);
        let at_48 = lag_at(48_000);
        assert!((28..=30).contains(&at_44), "44.1 kHz lag {at_44}");
        assert!((30..=33).contains(&at_48), "48 kHz lag {at_48}");
        assert!(at_44 < at_48);
        // Both tables share one tap count.
        assert_eq!(
            sphere.resampled_to(HRIR_ALT_SAMPLE_RATE).hrir_len(),
            sphere.hrir_len()
        );
    }

    /// Interaural lag of a right-hand noise source on a convolver over `spectra` run at `rate`.
    fn noise_lag_at(spectra: &Arc<SphereSpectra>, rate: u32) -> (i64, u32) {
        let mut convolver = Convolver::new(Arc::clone(spectra));
        let table = convolver.select_rate(rate);
        let mut state = 7u32;
        let (mut left, mut right) = (Vec::new(), Vec::new());
        for _ in 0..24 {
            let source: Vec<f32> = (0..CHUNK)
                .map(|_| {
                    state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    (state >> 8) as f32 / (1u32 << 24) as f32 - 0.5
                })
                .collect();
            let mut out = vec![(0.0f32, 0.0f32); CHUNK];
            convolver.process(&source, &mut out, Vec3::X, Vec3::X);
            left.extend(out.iter().map(|o| o.0));
            right.extend(out.iter().map(|o| o.1));
        }
        // Wider search than `lag`: 192 kHz puts the far ear ~126 samples late.
        let mut best = (0i64, f32::MIN);
        for lag in -160i64..=160 {
            let sum: f32 = (160..left.len() - 160)
                .map(|i| right[i] * left[(i as i64 + lag) as usize])
                .sum();
            if sum > best.1 {
                best = (lag, sum);
            }
        }
        (best.0, table)
    }

    /// AU-14 (ADR-7461): a table at a high mixer rate keeps every resampled tap, so the
    /// interaural delay keeps its length in time: ≈ 31.5 samples at 48 kHz, 63 at 96 kHz,
    /// 126 at 192 kHz. Without the table the mixer falls back to the nearest one (48 kHz).
    #[test]
    fn a_96k_table_doubles_the_interaural_lag_and_the_fallback_is_the_nearest_table() {
        let spectra = HrirSphere::synthesized().spectra();
        let (fallback, table) = noise_lag_at(&spectra, 96_000);
        assert_eq!(table, HRIR_SAMPLE_RATE, "no 96 kHz table yet: the nearest one");
        assert!((30..=33).contains(&fallback), "fallback lag {fallback}");
        let grown = spectra.with_rate(96_000).with_rate(192_000);
        assert_eq!(
            grown.rates(),
            vec![HRIR_SAMPLE_RATE, HRIR_ALT_SAMPLE_RATE, 96_000, 192_000]
        );
        assert!(!spectra.has_rate(96_000), "the original spectra are unchanged");
        let (at_48, _) = noise_lag_at(&grown, 48_000);
        let (at_96, table) = noise_lag_at(&grown, 96_000);
        let (at_192, table_192) = noise_lag_at(&grown, 192_000);
        assert_eq!((table, table_192), (96_000, 192_000));
        assert!(
            (at_96 - 2 * at_48).abs() <= 2,
            "96 kHz lag {at_96}, 48 kHz {at_48}"
        );
        assert!(
            (at_192 - 4 * at_48).abs() <= 4,
            "192 kHz lag {at_192}, 48 kHz {at_48}"
        );
    }

    #[test]
    fn high_rate_tables_keep_every_tap_and_a_smooth_fft_size() {
        let spectra = HrirSphere::synthesized().spectra().with_rate(96_000);
        let t = &spectra.tables[spectra.table_for(96_000)];
        // 97 taps at 48 kHz are 194 at 96 kHz; zero-padded so 32 + taps − 1 = 256.
        assert_eq!((t.hrir_len, t.pad_len), (225, 256));
        let t = &spectra.tables[spectra.table_for(HRIR_ALT_SAMPLE_RATE)];
        assert_eq!((t.hrir_len, t.pad_len), (HRIR_LEN, 128), "44.1 kHz unchanged");
        let t = &spectra.tables[spectra.table_for(HRIR_SAMPLE_RATE)];
        assert_eq!((t.hrir_len, t.pad_len), (HRIR_LEN, 128), "48 kHz unchanged");
        let at_192 = HrirSphere::synthesized().spectra().with_rate(192_000);
        let t = &at_192.tables[at_192.table_for(192_000)];
        assert!(t.pad_len.is_multiple_of(HRTF_BLOCK_LEN) && t.hrir_len >= 4 * HRIR_LEN);
        // A low mixdown rate keeps the 48 kHz tap count (as the 44.1 kHz table does).
        let low = HrirSphere::synthesized().spectra().with_rate(22_050);
        let t = &low.tables[low.table_for(22_050)];
        assert_eq!(t.hrir_len, HRIR_LEN);
    }

    /// The memory bound of ADR-7461: at most [`HRTF_MAX_RATE_TABLES`] tables; past the cap the
    /// spectra are returned unchanged and a mixer uses the nearest table.
    #[test]
    fn rate_tables_stop_at_the_cap() {
        let mut spectra = HrirSphere::synthesized().spectra();
        let base_bytes = spectra.table_bytes();
        for rate in [88_200, 96_000, 176_400, 192_000, 32_000, 22_050] {
            spectra = spectra.with_rate(rate);
        }
        assert_eq!(spectra.rates().len(), HRTF_MAX_RATE_TABLES);
        assert!(!spectra.has_rate(32_000) && !spectra.has_rate(22_050));
        assert_eq!(spectra.table_rate_for(32_000), HRIR_ALT_SAMPLE_RATE);
        let again = spectra.with_rate(32_000);
        assert!(Arc::ptr_eq(&again, &spectra), "a full set is returned as is");
        assert!(
            Arc::ptr_eq(&spectra.with_rate(96_000), &spectra),
            "an existing rate too"
        );
        // Synthesized sphere: 2 × 331,776 B at 44.1/48 kHz; all six tables stay under 4 MB.
        assert_eq!(base_bytes, 2 * 162 * 2 * 128 * 8);
        assert!(spectra.table_bytes() < 4 << 20, "{} bytes", spectra.table_bytes());
    }

    /// A convolver built over grown spectra and run at 48 kHz gives the same samples as one
    /// over the original two tables (its buffers are larger, the 48 kHz table is the same).
    #[test]
    fn extra_tables_do_not_change_the_48k_output() {
        let sphere = HrirSphere::synthesized();
        let plain = sphere.spectra();
        let grown = plain.with_rate(192_000);
        let render = |spectra: &Arc<SphereSpectra>, rate: u32| {
            let mut convolver = Convolver::new(Arc::clone(spectra));
            convolver.select_rate(rate);
            let mut out_all = Vec::new();
            for chunk in 0..8 {
                let source: Vec<f32> = (0..CHUNK)
                    .map(|i| (((chunk * CHUNK + i) * 37 % 101) as f32 / 50.0) - 1.0)
                    .collect();
                let mut out = vec![(0.0f32, 0.0f32); CHUNK];
                let a = chunk as f32 * 0.4;
                convolver.process(&source, &mut out, Vec3::new(a.sin(), 0.2, -a.cos()), Vec3::X);
                out_all.extend(out.iter().flat_map(|o| [o.0.to_bits(), o.1.to_bits()]));
            }
            out_all
        };
        assert_eq!(render(&plain, 48_000), render(&grown, 48_000));
        assert_eq!(render(&plain, 44_100), render(&grown, 44_100));
    }

    #[test]
    fn the_effect_reports_its_mixer_and_table_rates() {
        let spectra = HrirSphere::synthesized().spectra();
        let control = Arc::new(HrtfControl::default());
        assert_eq!((control.mixer_rate(), control.table_rate()), (None, None));
        let (mut effect, ()) = HrtfEffectBuilder::new(Arc::clone(&spectra), Arc::clone(&control)).build();
        effect.init(96_000, 1024);
        assert_eq!(control.mixer_rate(), Some(96_000));
        assert_eq!(control.table_rate(), Some(HRIR_SAMPLE_RATE));
        assert_eq!(control.stale_rate(), Some(96_000));
        let control = Arc::new(HrtfControl::default());
        let (mut effect, ()) =
            HrtfEffectBuilder::new(spectra.with_rate(96_000), Arc::clone(&control)).build();
        effect.init(96_000, 1024);
        assert_eq!(control.table_rate(), Some(96_000));
        assert_eq!(control.stale_rate(), None);
        effect.on_change_sample_rate(44_100);
        assert_eq!(
            (control.mixer_rate(), control.table_rate()),
            (Some(44_100), Some(44_100))
        );
    }

    #[test]
    fn every_direction_finds_a_face() {
        let sphere = HrirSphere::synthesized();
        let spectra = sphere.spectra();
        let mut dirs = vec![Vec3::X, Vec3::NEG_X, Vec3::Y, Vec3::NEG_Y, Vec3::Z, Vec3::NEG_Z];
        dirs.extend(sphere.points().iter().copied());
        for i in 0..500 {
            let a = i as f32 * 0.7;
            let e = (i as f32 * 0.31).sin() * 1.55;
            dirs.push(Vec3::new(a.sin() * e.cos(), e.sin(), -a.cos() * e.cos()));
        }
        for dir in dirs {
            assert!(spectra.locate(dir.normalize(), 0).is_some(), "{dir} hits no face");
        }
    }

    #[test]
    fn files_round_trip_and_resample() {
        let sphere = HrirSphere::synthesized();
        let bytes = sphere.to_bytes();
        assert!(validate_sphere_bytes(&bytes).is_ok());
        let back = HrirSphere::from_bytes(&bytes).expect("reload");
        assert_eq!(back.hrir_len(), HRIR_LEN, "no resampling at the native rate");
        assert_eq!(back.points().len(), sphere.points().len());
        let (a, _) = back.hrirs(7).expect("point");
        let (b, _) = sphere.hrirs(7).expect("point");
        assert_eq!(a, b);
        // The same file declared at 24 kHz is upsampled 2× without shifting the arrival.
        let mut slow = bytes.clone();
        slow[4..8].copy_from_slice(&24_000u32.to_le_bytes());
        let up = HrirSphere::from_bytes(&slow).expect("resampled");
        assert_eq!(up.hrir_len(), HRIR_LEN * 2);
        let (u, _) = up.hrirs(7).expect("point");
        assert!((arrival(u) as i64 - 2 * arrival(b) as i64).abs() <= 1);
    }

    /// Builds a minimal closed tetrahedron sphere at `rate`: 4 points, 4 triangles, `taps`-tap
    /// HRIRs (the left one of point 0 is `left0`, everything else silence).
    fn tetrahedron_bytes(rate: u32, taps: u32, left0: &[f32]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"HRIR");
        for v in [rate, taps, 4, 12] {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        // Every edge of the tetrahedron is shared by exactly two faces (a closed surface).
        for face in [[0u32, 1, 2], [0, 1, 3], [0, 2, 3], [1, 2, 3]] {
            for index in face {
                bytes.extend_from_slice(&index.to_le_bytes());
            }
        }
        let points = [
            Vec3::new(1.0, 1.0, 1.0),
            Vec3::new(1.0, -1.0, -1.0),
            Vec3::new(-1.0, 1.0, -1.0),
            Vec3::new(-1.0, -1.0, 1.0),
        ];
        for point in points {
            for c in [point.x, point.y, point.z] {
                bytes.extend_from_slice(&c.to_le_bytes());
            }
            // The layout stores each point's whole left HRIR, then its right HRIR.
            for i in 0..taps as usize {
                let l = left0.get(i).copied().unwrap_or(0.0);
                bytes.extend_from_slice(&l.to_le_bytes());
            }
            for _ in 0..taps {
                bytes.extend_from_slice(&0.0f32.to_le_bytes());
            }
        }
        bytes
    }

    #[test]
    fn integer_ratio_files_keep_every_input_tap() {
        // A 96 kHz file whose left HRIR is an impulse on an *odd* tap: the naive linear
        // downsample reads only ir[2n] and drops it entirely (some directions would load
        // silently broken, others +6 dB hot). The band-limited load keeps it.
        let mut ir = vec![0.0f32; 16];
        ir[7] = 1.0;
        ir[13] = 0.5;
        let bytes = tetrahedron_bytes(96_000, 16, &ir);
        assert!(validate_sphere_bytes(&bytes).is_ok());
        let sphere = HrirSphere::from_bytes(&bytes).expect("loads");
        let (left, _) = sphere.hrirs(0).expect("point 0");
        assert_eq!(left, resample_hrir(&ir, 96_000, HRIR_SAMPLE_RATE));
        assert!(
            left.iter().any(|s| s.abs() > 0.1),
            "odd-tap content survives the exact 2:1 downsample: {left:?}"
        );
    }

    /// Rev-kimi-28 (audio2-k item 1): resampling an HRIR keeps its DC gain (the tap sum),
    /// as the linear loader did (`from / to`). `resample_sinc` keeps the amplitude of a
    /// signal, so the tap sum of an impulse response scales with the tap count: unscaled, a
    /// 96 kHz file loaded at -6 dB, a 44.1 kHz one at +0.7 dB and a 24 kHz one at +6 dB.
    #[test]
    fn resampled_hrirs_keep_their_dc_gain() {
        // A smooth 256-tap pulse (a Hann bump), well inside the band of every rate below.
        let taps = 256;
        let ir: Vec<f32> = (0..taps)
            .map(|i| {
                let x = (i as f32 - 64.0) / 24.0;
                if x.abs() < 1.0 {
                    0.5 * (1.0 + (std::f32::consts::PI * x).cos())
                } else {
                    0.0
                }
            })
            .collect();
        let sum_in: f32 = ir.iter().sum();
        for rate in [96_000u32, 44_100, 24_000] {
            let bytes = tetrahedron_bytes(rate, taps, &ir);
            let sphere = HrirSphere::from_bytes(&bytes).expect("loads");
            let (left, _) = sphere.hrirs(0).expect("point 0");
            let ratio = left.iter().sum::<f32>() / sum_in;
            assert!(
                (ratio - 1.0).abs() < 0.01,
                "a {rate} Hz file keeps its DC gain at {HRIR_SAMPLE_RATE} Hz: tap sum x{ratio}"
            );
        }
        // The 44.1 kHz table the convolver uses on a 44.1 kHz mixer keeps it too.
        let native = HrirSphere::from_bytes(&tetrahedron_bytes(HRIR_SAMPLE_RATE, taps, &ir)).expect("loads");
        let alt = native.resampled_to(HRIR_ALT_SAMPLE_RATE);
        let (left, _) = alt.hrirs(0).expect("point 0");
        let ratio = left.iter().sum::<f32>() / sum_in;
        assert!(
            (ratio - 1.0).abs() < 0.01,
            "the {HRIR_ALT_SAMPLE_RATE} Hz table keeps the DC gain: tap sum x{ratio}"
        );
    }

    #[test]
    fn sphere_validation_rejects_broken_files() {
        let good = HrirSphere::synthesized().to_bytes();
        assert!(validate_sphere_bytes(b"RIFF....").is_err());
        assert!(
            validate_sphere_bytes(&good[..good.len() / 2]).is_err(),
            "truncated"
        );
        // Drop the last face: the surface has a hole.
        let mut holed = good.clone();
        let index_count = u32::from_le_bytes([holed[16], holed[17], holed[18], holed[19]]);
        holed[16..20].copy_from_slice(&(index_count - 3).to_le_bytes());
        holed.drain(20 + (index_count as usize - 3) * 4..20 + index_count as usize * 4);
        let err = validate_sphere_bytes(&holed).expect_err("hole");
        assert!(err.contains("not closed"), "{err}");
        assert!(HrirSphere::from_bytes(&holed).is_err());
    }

    #[test]
    fn control_rejects_degenerate_directions() {
        let control = HrtfControl::default();
        control.set(Vec3::ZERO, f32::NAN);
        assert_eq!(control.get(), (Vec3::NEG_Z, 1.0));
        control.set(Vec3::new(3.0, 0.0, 0.0), 0.25);
        assert_eq!(control.get(), (Vec3::X, 0.25));
    }
}
