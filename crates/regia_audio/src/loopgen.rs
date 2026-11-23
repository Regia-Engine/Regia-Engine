// SPDX-License-Identifier: MPL-2.0
//! Procedural loop generator: `.loopgen.ron` v1 (`docs/formats/loopgen-ron.md`, ADR-299).
//!
//! A [`LoopGenDef`] is a small graph of generators (noise, oscillators, filtered-noise rumble,
//! amplitude modulation, filters, gain, mix). [`LoopGenDef::render`] is a **pure function of the
//! definition**: the same bytes produce the same samples on every platform. The noise path uses
//! integer random streams ([`Rng`]) and the filters and oscillators use only `+ - * /`, `floor`
//! and `sqrt` (all exactly rounded by IEEE 754) plus the polynomial approximations in this module
//! (`det_sin_cycles`, `det_exp2`), never `f32::sin`/`exp`, whose last bits differ between libm
//! implementations.
//!
//! The loop is seamless: the graph is rendered a crossfade length past the end, and the head of
//! the loop is blended (equal power) with that continuation, so the last sample flows into the
//! first one.
//!
//! Rendered PCM is cached in memory by [`LoopGenCache`], keyed by the file's content hash. A
//! layer of an [`AudioPatch`](crate::AudioPatch) can name a `.loopgen.ron` as its `sound`
//! ([`AudioPatch::from_def_with_loopgen`](crate::AudioPatch::from_def_with_loopgen)).

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use kira::sound::static_sound::{StaticSoundData, StaticSoundSettings};
use regia_core::math::Rng;
use regia_core::version;
use serde::{Deserialize, Serialize};

use crate::error::AudioError;
use crate::sound::SoundAsset;

/// The loopgen format version this engine reads.
pub const LOOPGEN_VERSION: u32 = 1;

/// The file suffix of a loop generator asset.
pub const LOOPGEN_SUFFIX: &str = ".loopgen.ron";

/// Longest loop a file may ask for, in seconds.
pub const MAX_LENGTH_SECS: f32 = 60.0;

/// Most nodes a graph may have.
pub const MAX_NODES: usize = 64;

/// Seconds rendered before the loop and thrown away, so filters and integrators are settled.
const WARMUP_SECS: f64 = 0.25;

const LOG2_10: f64 = std::f64::consts::LOG2_10;
const LOG2_E: f64 = std::f64::consts::LOG2_E;
const TAU: f64 = std::f64::consts::TAU;

/// True when `path` names a loop generator asset (`*.loopgen.ron`, any case).
pub fn is_loopgen_path(path: &str) -> bool {
    path.to_ascii_lowercase().ends_with(LOOPGEN_SUFFIX)
}

// ------------------------------------------------------------------------------- format types

/// The color of a [`LoopNode::Noise`] source.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NoiseColor {
    /// Flat spectrum.
    White,
    /// −3 dB per octave (Paul Kellet's filter, `docs/provenance.md`).
    Pink,
    /// −6 dB per octave (leaky integrated white noise).
    Brown,
}

/// The waveform of a [`LoopNode::Osc`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OscShape {
    /// Sine.
    Sine,
    /// Rising saw (naive, not band-limited).
    Saw,
    /// Square (naive, not band-limited).
    Square,
    /// Triangle.
    Triangle,
}

/// The response of a [`LoopNode::Filter`] biquad (RBJ cookbook).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FilterMode {
    /// Low-pass.
    LowPass,
    /// High-pass.
    HighPass,
    /// Band-pass with constant 0 dB peak gain.
    BandPass,
}

/// The response of a [`LoopNode::OnePole`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OnePoleMode {
    /// Low-pass.
    LowPass,
    /// High-pass.
    HighPass,
}

fn one_f32() -> f32 {
    1.0
}

fn default_q() -> f32 {
    std::f32::consts::FRAC_1_SQRT_2
}

/// One generator or processor of the graph.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum LoopNode {
    /// A noise source, amplitude ±`gain` (white) before coloring.
    Noise {
        /// White, pink or brown.
        color: NoiseColor,
        /// Output gain (linear).
        #[serde(default = "one_f32")]
        gain: f32,
    },
    /// An oscillator.
    Osc {
        /// Waveform.
        shape: OscShape,
        /// Frequency in Hz.
        hz: f32,
        /// Detune in cents (1200 = one octave up).
        #[serde(default)]
        detune_cents: f32,
        /// Output gain (linear).
        #[serde(default = "one_f32")]
        gain: f32,
        /// Starting phase in cycles (0..1).
        #[serde(default)]
        phase: f32,
    },
    /// Low-passed white noise, scaled so its level does not fall with the cutoff.
    Rumble {
        /// Low-pass cutoff in Hz.
        cutoff_hz: f32,
        /// Low-pass resonance.
        #[serde(default = "default_q")]
        q: f32,
        /// Output gain (linear).
        #[serde(default = "one_f32")]
        gain: f32,
    },
    /// Amplitude modulation: `input × (1 − depth + depth × lfo)`, `lfo` a 0..1 sine.
    Am {
        /// Node to modulate.
        input: String,
        /// Modulation rate in Hz.
        rate_hz: f32,
        /// Depth 0..1 (0 = no change, 1 = full tremolo).
        depth: f32,
    },
    /// A biquad filter.
    Filter {
        /// Node to filter.
        input: String,
        /// Response.
        mode: FilterMode,
        /// Corner or centre frequency in Hz.
        cutoff_hz: f32,
        /// Quality factor.
        #[serde(default = "default_q")]
        q: f32,
    },
    /// A one-pole filter (6 dB per octave).
    OnePole {
        /// Node to filter.
        input: String,
        /// Response.
        mode: OnePoleMode,
        /// Corner frequency in Hz.
        cutoff_hz: f32,
    },
    /// A gain in dB.
    Gain {
        /// Node to scale.
        input: String,
        /// Gain in dB.
        db: f32,
    },
    /// A weighted sum of nodes.
    Mix {
        /// The summed inputs.
        inputs: Vec<LoopMixInput>,
    },
}

impl LoopNode {
    fn variant(&self) -> &'static str {
        match self {
            Self::Noise { .. } => "Noise",
            Self::Osc { .. } => "Osc",
            Self::Rumble { .. } => "Rumble",
            Self::Am { .. } => "Am",
            Self::Filter { .. } => "Filter",
            Self::OnePole { .. } => "OnePole",
            Self::Gain { .. } => "Gain",
            Self::Mix { .. } => "Mix",
        }
    }

    /// The input references of the node: `(field path inside the variant, referenced node id)`.
    fn inputs(&self) -> Vec<(String, &str)> {
        match self {
            Self::Am { input, .. }
            | Self::Filter { input, .. }
            | Self::OnePole { input, .. }
            | Self::Gain { input, .. } => vec![("input".to_string(), input.as_str())],
            Self::Mix { inputs } => inputs
                .iter()
                .enumerate()
                .map(|(i, m)| (format!("inputs[{i}].input"), m.input.as_str()))
                .collect(),
            Self::Noise { .. } | Self::Osc { .. } | Self::Rumble { .. } => Vec::new(),
        }
    }
}

/// One weighted input of a [`LoopNode::Mix`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LoopMixInput {
    /// Id of the node to sum.
    pub input: String,
    /// Linear gain.
    #[serde(default = "one_f32")]
    pub gain: f32,
}

/// A named node of the graph.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LoopNodeDef {
    /// Unique id other nodes (and `output`) refer to.
    pub id: String,
    /// What the node does.
    pub node: LoopNode,
}

fn default_rate() -> u32 {
    48_000
}

fn default_channels() -> u32 {
    1
}

fn default_crossfade() -> f32 {
    0.1
}

/// The RON form of a loop generator (`.loopgen.ron`), version 1.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LoopGenDef {
    /// Format version; must be 1.
    pub version: u32,
    /// Name, used in logs and the agent protocol.
    pub name: String,
    /// Sample rate in Hz (8000..=96000). Default 48000.
    #[serde(default = "default_rate")]
    pub sample_rate: u32,
    /// Loop length in seconds (0.05..=60).
    pub length_secs: f32,
    /// 1 (mono) or 2 (stereo: each channel draws its own noise streams). Default 1.
    #[serde(default = "default_channels")]
    pub channels: u32,
    /// Seed of the noise streams.
    #[serde(default)]
    pub seed: u64,
    /// Length of the wrap-around crossfade in seconds (0 = none; at most half the loop).
    #[serde(default = "default_crossfade")]
    pub crossfade_secs: f32,
    /// Final gain in dB (the result is hard-limited to ±1). Default 0.
    #[serde(default)]
    pub output_gain_db: f32,
    /// The graph nodes.
    pub nodes: Vec<LoopNodeDef>,
    /// Id of the node that is the loop's output.
    pub output: String,
}

/// An invalid loop generator, with the key path of the offending value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoopGenError {
    /// Where in the file the problem is (`nodes[2].node.Filter.cutoff_hz`).
    pub key_path: String,
    /// What is wrong.
    pub message: String,
}

impl std::fmt::Display for LoopGenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.key_path, self.message)
    }
}

impl std::error::Error for LoopGenError {}

impl From<LoopGenError> for AudioError {
    fn from(error: LoopGenError) -> Self {
        AudioError::load("loopgen", error)
    }
}

fn bad(path: impl Into<String>, message: impl Into<String>) -> LoopGenError {
    LoopGenError {
        key_path: path.into(),
        message: message.into(),
    }
}

fn check_range(path: &str, value: f32, lo: f32, hi: f32) -> Result<(), LoopGenError> {
    if (lo..=hi).contains(&value) {
        Ok(())
    } else {
        Err(bad(path, format!("{value} is outside {lo}..={hi}")))
    }
}

/// The serde field-list fingerprint of the `.loopgen.ron` format (hashed by `cargo xtask
/// check-formats`). Struct fields, enum variant names, and the fields of the [`LoopNode`] struct
/// variants (listed by hand: serde cannot enumerate them) are all included.
pub fn loopgen_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names, serde_variant_names};
    let node_fields: Vec<String> = [
        "Noise.color",
        "Noise.gain",
        "Osc.shape",
        "Osc.hz",
        "Osc.detune_cents",
        "Osc.gain",
        "Osc.phase",
        "Rumble.cutoff_hz",
        "Rumble.q",
        "Rumble.gain",
        "Am.input",
        "Am.rate_hz",
        "Am.depth",
        "Filter.input",
        "Filter.mode",
        "Filter.cutoff_hz",
        "Filter.q",
        "OnePole.input",
        "OnePole.mode",
        "OnePole.cutoff_hz",
        "Gain.input",
        "Gain.db",
        "Mix.inputs",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect();
    join_field_lists(&[
        serde_field_names::<LoopGenDef>(),
        serde_field_names::<LoopNodeDef>(),
        serde_field_names::<LoopMixInput>(),
        serde_variant_names::<LoopNode>(),
        node_fields,
        serde_variant_names::<NoiseColor>(),
        serde_variant_names::<OscShape>(),
        serde_variant_names::<FilterMode>(),
        serde_variant_names::<OnePoleMode>(),
    ])
}

/// The versions a built-in migration step leaves (`from` side): none, v1 is the first version.
pub fn loopgen_migration_sources() -> Vec<u32> {
    Vec::new()
}

// ---------------------------------------------------------------- deterministic math helpers

/// `sin(2π · cycles)` from a polynomial: only `+ - * floor`, so bit-identical on every platform.
/// Absolute error below 1e-7.
fn det_sin_cycles(cycles: f64) -> f64 {
    let mut p = cycles - cycles.floor();
    let mut sign = 1.0;
    if p > 0.5 {
        p -= 0.5;
        sign = -1.0;
    }
    if p > 0.25 {
        p = 0.5 - p;
    }
    let a = TAU * p;
    let a2 = a * a;
    let poly = 1.0
        + a2 * (-1.0 / 6.0
            + a2 * (1.0 / 120.0
                + a2 * (-1.0 / 5_040.0 + a2 * (1.0 / 362_880.0 + a2 * (-1.0 / 39_916_800.0)))));
    sign * a * poly
}

/// `cos(2π · cycles)`.
fn det_cos_cycles(cycles: f64) -> f64 {
    det_sin_cycles(cycles + 0.25)
}

/// `2^x` from a Taylor polynomial of the fractional part and an exponent-field scale: only
/// `+ - * floor` and a bit cast, so bit-identical on every platform. Relative error below 1e-11.
fn det_exp2(x: f64) -> f64 {
    let x = x.clamp(-1000.0, 1000.0);
    let whole = x.floor();
    let u = (x - whole) * std::f64::consts::LN_2;
    let mut sum = 1.0;
    let mut term = 1.0;
    for k in 1..=13 {
        term *= u / f64::from(k);
        sum += term;
    }
    let scale = f64::from_bits(((whole as i64 + 1023) as u64) << 52);
    sum * scale
}

/// `e^x` via [`det_exp2`].
fn det_exp(x: f64) -> f64 {
    det_exp2(x * LOG2_E)
}

/// Linear gain of `db` decibels.
fn db_to_linear(db: f64) -> f64 {
    det_exp2(db * LOG2_10 / 20.0)
}

/// Uniform white noise in `[-1, 1)` from the integer stream.
fn white(rng: &mut Rng) -> f64 {
    f64::from(rng.next_u32() as i32) * (1.0 / 2_147_483_648.0)
}

fn fnv1a(bytes: impl IntoIterator<Item = u8>) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

// ------------------------------------------------------------------------------ DSP blocks

/// RBJ biquad coefficients, normalised by `a0` (Audio EQ Cookbook).
#[derive(Clone, Copy, Debug)]
struct BiquadCoeffs {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
}

impl BiquadCoeffs {
    fn new(mode: FilterMode, cutoff_hz: f64, q: f64, sample_rate: f64) -> Self {
        let cycles = cutoff_hz / sample_rate;
        let cos = det_cos_cycles(cycles);
        let sin = det_sin_cycles(cycles);
        let alpha = sin / (2.0 * q);
        let (b0, b1, b2) = match mode {
            FilterMode::LowPass => ((1.0 - cos) / 2.0, 1.0 - cos, (1.0 - cos) / 2.0),
            FilterMode::HighPass => ((1.0 + cos) / 2.0, -(1.0 + cos), (1.0 + cos) / 2.0),
            FilterMode::BandPass => (alpha, 0.0, -alpha),
        };
        let a0 = 1.0 + alpha;
        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: -2.0 * cos / a0,
            a2: (1.0 - alpha) / a0,
        }
    }
}

/// A direct-form I biquad (state in `f64`).
#[derive(Clone, Copy, Debug)]
struct Biquad {
    c: BiquadCoeffs,
    x1: f64,
    x2: f64,
    y1: f64,
    y2: f64,
}

impl Biquad {
    fn new(c: BiquadCoeffs) -> Self {
        Self {
            c,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    fn process(&mut self, x: f64) -> f64 {
        let c = self.c;
        let y = c.b0 * x + c.b1 * self.x1 + c.b2 * self.x2 - c.a1 * self.y1 - c.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }
}

/// Paul Kellet's refined pink-noise filter (state in `f64`).
#[derive(Default)]
struct PinkState {
    b: [f64; 7],
}

impl PinkState {
    fn process(&mut self, w: f64) -> f64 {
        let b = &mut self.b;
        b[0] = 0.998_86 * b[0] + w * 0.055_517_9;
        b[1] = 0.993_32 * b[1] + w * 0.075_075_9;
        b[2] = 0.969_00 * b[2] + w * 0.153_852_0;
        b[3] = 0.866_50 * b[3] + w * 0.310_485_6;
        b[4] = 0.550_00 * b[4] + w * 0.532_952_2;
        b[5] = -0.761_6 * b[5] - w * 0.016_898_0;
        let pink = b[0] + b[1] + b[2] + b[3] + b[4] + b[5] + b[6] + w * 0.536_2;
        b[6] = w * 0.115_926;
        pink * 0.11
    }
}

fn osc_value(shape: OscShape, phase: f64) -> f64 {
    match shape {
        OscShape::Sine => det_sin_cycles(phase),
        OscShape::Saw => 2.0 * phase - 1.0,
        OscShape::Square => {
            if phase < 0.5 {
                1.0
            } else {
                -1.0
            }
        }
        OscShape::Triangle => 1.0 - 4.0 * (phase - 0.5).abs(),
    }
}

fn to_f32(value: f64) -> f32 {
    let v = value as f32;
    if v.is_finite() { v } else { 0.0 }
}

// ----------------------------------------------------------------------------------- render

/// The PCM a loop generator renders to, plus its measurements.
#[derive(Clone, Debug, PartialEq)]
pub struct RenderedLoop {
    /// Name of the definition.
    pub name: String,
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Channels (1 or 2).
    pub channels: u32,
    /// Interleaved samples, `frames × channels`.
    pub samples: Arc<[f32]>,
}

/// Measurements of a rendered loop (`audio.loopgen_render` returns them).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LoopStats {
    /// Frames (samples per channel).
    pub frames: usize,
    /// Length in seconds.
    pub duration_secs: f64,
    /// Root mean square over all channels.
    pub rms: f32,
    /// Largest absolute sample.
    pub peak: f32,
    /// Largest `|first − last|` over the channels: the jump when the loop wraps.
    pub seam_jump: f32,
    /// Largest `|s[i+1] − s[i]|` inside the loop; a seamless loop has `seam_jump <= max_step`.
    pub max_step: f32,
    /// FNV-1a 64 hash of the sample bits (determinism check).
    pub hash: u64,
}

impl RenderedLoop {
    /// Frames (samples per channel).
    pub fn frames(&self) -> usize {
        self.samples.len() / self.channels.max(1) as usize
    }

    /// Measures the loop.
    pub fn stats(&self) -> LoopStats {
        let channels = self.channels.max(1) as usize;
        let frames = self.frames();
        let mut sum_sq = 0.0f64;
        let mut peak = 0.0f32;
        for &s in self.samples.iter() {
            sum_sq += f64::from(s) * f64::from(s);
            peak = peak.max(s.abs());
        }
        let mut seam_jump = 0.0f32;
        let mut max_step = 0.0f32;
        if frames > 1 {
            for c in 0..channels {
                let at = |frame: usize| self.samples[frame * channels + c];
                seam_jump = seam_jump.max((at(0) - at(frames - 1)).abs());
                for frame in 1..frames {
                    max_step = max_step.max((at(frame) - at(frame - 1)).abs());
                }
            }
        }
        let rms = if self.samples.is_empty() {
            0.0
        } else {
            (sum_sq / self.samples.len() as f64).sqrt() as f32
        };
        let hash = fnv1a(
            self.samples
                .iter()
                .flat_map(|s| s.to_bits().to_le_bytes())
                .chain(self.sample_rate.to_le_bytes())
                .chain(self.channels.to_le_bytes()),
        );
        LoopStats {
            frames,
            duration_secs: frames as f64 / f64::from(self.sample_rate.max(1)),
            rms,
            peak,
            seam_jump,
            max_step,
            hash,
        }
    }

    /// The loop as a static [`SoundAsset`] (looped by the patch that plays it).
    pub fn to_sound_asset(&self) -> SoundAsset {
        let channels = self.channels.max(1) as usize;
        let frames: Vec<kira::Frame> = self
            .samples
            .chunks(channels)
            .map(|f| match f {
                [l, r] => kira::Frame { left: *l, right: *r },
                [m, ..] => kira::Frame::from_mono(*m),
                [] => kira::Frame::ZERO,
            })
            .collect();
        SoundAsset::from_static_data(StaticSoundData {
            sample_rate: self.sample_rate,
            frames: frames.into(),
            slice: None,
            settings: StaticSoundSettings::new(),
        })
    }

    /// A 16-bit PCM WAV file of the loop.
    pub fn to_wav_bytes(&self) -> Vec<u8> {
        let data_len = (self.samples.len() * 2) as u32;
        let channels = self.channels.max(1) as u16;
        let mut wav = Vec::with_capacity(44 + data_len as usize);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_len).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&channels.to_le_bytes());
        wav.extend_from_slice(&self.sample_rate.to_le_bytes());
        wav.extend_from_slice(&(self.sample_rate * u32::from(channels) * 2).to_le_bytes());
        wav.extend_from_slice(&(channels * 2).to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_len.to_le_bytes());
        for &s in self.samples.iter() {
            let q = (f64::from(s.clamp(-1.0, 1.0)) * 32_767.0).round() as i16;
            wav.extend_from_slice(&q.to_le_bytes());
        }
        wav
    }
}

impl LoopGenDef {
    /// Parses a definition from RON text: version check first (a newer file is rejected even
    /// when its body no longer matches), then the field-level checks of [`Self::validate`].
    /// A syntax error has the key path `(file)`.
    pub fn parse(text: &str) -> Result<Self, LoopGenError> {
        let found = version::peek_ron_version(text).map_err(|e| bad("(file)", e))?;
        if found > LOOPGEN_VERSION {
            return Err(bad(
                "version",
                format!("{found} is newer than this engine reads ({LOOPGEN_VERSION})"),
            ));
        }
        let def: Self = ron::from_str(text).map_err(|e| bad("(file)", e.to_string()))?;
        def.validate()?;
        Ok(def)
    }

    /// [`Self::parse`] with the error as an [`AudioError`].
    pub fn from_ron(text: &str) -> Result<Self, AudioError> {
        Self::parse(text).map_err(AudioError::from)
    }

    /// Checks every field; the error names the key path.
    pub fn validate(&self) -> Result<(), LoopGenError> {
        self.analyse().map(|_| ())
    }

    fn frames(&self) -> usize {
        (f64::from(self.length_secs) * f64::from(self.sample_rate)).round() as usize
    }

    fn crossfade_frames(&self) -> usize {
        (f64::from(self.crossfade_secs) * f64::from(self.sample_rate)).round() as usize
    }

    /// Validates and returns the evaluation order of the nodes the output depends on.
    fn analyse(&self) -> Result<Vec<usize>, LoopGenError> {
        if self.version != LOOPGEN_VERSION {
            return Err(bad(
                "version",
                format!("is {}, this engine reads {LOOPGEN_VERSION}", self.version),
            ));
        }
        if !(8_000..=96_000).contains(&self.sample_rate) {
            return Err(bad("sample_rate", "must be 8000..=96000"));
        }
        check_range("length_secs", self.length_secs, 0.05, MAX_LENGTH_SECS)?;
        if !(1..=2).contains(&self.channels) {
            return Err(bad("channels", "must be 1 or 2"));
        }
        check_range("crossfade_secs", self.crossfade_secs, 0.0, self.length_secs / 2.0)?;
        check_range("output_gain_db", self.output_gain_db, -96.0, 24.0)?;
        if self.nodes.is_empty() || self.nodes.len() > MAX_NODES {
            return Err(bad("nodes", format!("needs 1..={MAX_NODES} nodes")));
        }
        let mut index: HashMap<&str, usize> = HashMap::new();
        for (i, def) in self.nodes.iter().enumerate() {
            if def.id.is_empty() {
                return Err(bad(format!("nodes[{i}].id"), "must not be empty"));
            }
            if index.insert(def.id.as_str(), i).is_some() {
                return Err(bad(
                    format!("nodes[{i}].id"),
                    format!("`{}` is used twice", def.id),
                ));
            }
        }
        let nyquist = self.sample_rate as f32 / 2.0;
        for (i, def) in self.nodes.iter().enumerate() {
            let base = format!("nodes[{i}].node.{}", def.node.variant());
            check_node(&def.node, &base, nyquist)?;
            for (field, target) in def.node.inputs() {
                if !index.contains_key(target) {
                    return Err(bad(format!("{base}.{field}"), format!("unknown node `{target}`")));
                }
            }
        }
        let Some(&output) = index.get(self.output.as_str()) else {
            return Err(bad("output", format!("unknown node `{}`", self.output)));
        };
        // Depth-first order from the output; 1 = on the stack, 2 = done.
        let mut state = vec![0u8; self.nodes.len()];
        let mut order = Vec::new();
        self.visit(output, &index, &mut state, &mut order)?;
        Ok(order)
    }

    fn visit(
        &self,
        node: usize,
        index: &HashMap<&str, usize>,
        state: &mut [u8],
        order: &mut Vec<usize>,
    ) -> Result<(), LoopGenError> {
        match state[node] {
            2 => return Ok(()),
            1 => {
                return Err(bad(
                    format!("nodes[{node}].id"),
                    format!("`{}` feeds itself (the graph has a cycle)", self.nodes[node].id),
                ));
            }
            _ => {}
        }
        state[node] = 1;
        for (_, target) in self.nodes[node].node.inputs() {
            if let Some(&next) = index.get(target) {
                self.visit(next, index, state, order)?;
            }
        }
        state[node] = 2;
        order.push(node);
        Ok(())
    }

    /// Renders the loop. A pure function of the definition (bit-identical on every platform).
    pub fn render(&self) -> Result<RenderedLoop, LoopGenError> {
        let order = self.analyse()?;
        let n = self.frames().max(1);
        let x = self.crossfade_frames().min(n / 2);
        let warm = (WARMUP_SECS * f64::from(self.sample_rate)) as usize;
        let total = warm + n + x;
        let gain = db_to_linear(f64::from(self.output_gain_db));
        let channels = self.channels as usize;

        let rendered: Vec<Vec<f32>> = (0..self.channels)
            .map(|channel| self.render_channel(&order, total, channel))
            .collect();

        let mut samples = vec![0.0f32; n * channels];
        for (c, buffer) in rendered.iter().enumerate() {
            let tail = &buffer[warm..];
            // How alike the head and the continuation are: equal-power gains keep the level of
            // unrelated content (noise), but sum alike content (a tone whose period divides the
            // loop) up to +3 dB in the middle of the wrap. ADR-872.
            let r = if x > 0 {
                wrap_correlation(&tail[..x], &tail[n..n + x])
            } else {
                0.0
            };
            for (i, frame) in samples.chunks_exact_mut(channels).enumerate() {
                let v = if i < x {
                    // Equal-power wrap: the continuation past the end fades out as the head
                    // fades in, so sample n-1 flows into sample 0. The pair is normalised by the
                    // power of a weighted sum of two signals with correlation r,
                    // `a² + b² + 2·r·a·b` (with a² + b² = 1), so the level holds for any content.
                    let t = (i as f64 + 0.5) / x as f64;
                    let fade_in = det_sin_cycles(t * 0.25);
                    let fade_out = det_sin_cycles(t * 0.25 + 0.25);
                    let norm = 1.0 / (1.0 + 2.0 * r * fade_in * fade_out).sqrt();
                    (f64::from(tail[i]) * fade_in + f64::from(tail[n + i]) * fade_out) * norm
                } else {
                    f64::from(tail[i])
                };
                frame[c] = to_f32((v * gain).clamp(-1.0, 1.0));
            }
        }
        Ok(RenderedLoop {
            name: self.name.clone(),
            sample_rate: self.sample_rate,
            channels: self.channels,
            samples: samples.into(),
        })
    }

    fn render_channel(&self, order: &[usize], total: usize, channel: u32) -> Vec<f32> {
        let index: HashMap<&str, usize> = self
            .nodes
            .iter()
            .enumerate()
            .map(|(i, n)| (n.id.as_str(), i))
            .collect();
        let mut last_use = vec![0usize; self.nodes.len()];
        for (pos, &node) in order.iter().enumerate() {
            for (_, target) in self.nodes[node].node.inputs() {
                if let Some(&j) = index.get(target) {
                    last_use[j] = pos;
                }
            }
        }
        let sample_rate = f64::from(self.sample_rate);
        let mut buffers: Vec<Vec<f32>> = vec![Vec::new(); self.nodes.len()];
        for (pos, &node) in order.iter().enumerate() {
            let stream = (u64::from(channel) << 32) | node as u64;
            let mut rng = Rng::with_stream(self.seed, stream ^ 0x4c4f_4f50_0000_0000);
            let out: Vec<f32> = match &self.nodes[node].node {
                LoopNode::Noise { color, gain } => render_noise(*color, f64::from(*gain), total, &mut rng),
                LoopNode::Osc {
                    shape,
                    hz,
                    detune_cents,
                    gain,
                    phase,
                } => {
                    let ratio = det_exp2(f64::from(*detune_cents) / 1_200.0);
                    let step = f64::from(*hz) * ratio / sample_rate;
                    let gain = f64::from(*gain);
                    let mut p = f64::from(*phase);
                    (0..total)
                        .map(|_| {
                            let v = osc_value(*shape, p) * gain;
                            p += step;
                            p -= p.floor();
                            to_f32(v)
                        })
                        .collect()
                }
                LoopNode::Rumble { cutoff_hz, q, gain } => {
                    let coeffs = BiquadCoeffs::new(
                        FilterMode::LowPass,
                        f64::from(*cutoff_hz),
                        f64::from(*q),
                        sample_rate,
                    );
                    let mut filter = Biquad::new(coeffs);
                    // Keeps the in-band level: white noise has its power spread to Nyquist.
                    let makeup = (sample_rate / 2.0 / f64::from(*cutoff_hz)).sqrt().min(32.0);
                    let gain = f64::from(*gain) * makeup;
                    (0..total)
                        .map(|_| to_f32(filter.process(white(&mut rng)) * gain))
                        .collect()
                }
                LoopNode::Am {
                    input: id,
                    rate_hz,
                    depth,
                } => {
                    let depth = f64::from(*depth);
                    let step = f64::from(*rate_hz) / sample_rate;
                    lookup(&index, &buffers, id)
                        .iter()
                        .enumerate()
                        .map(|(i, &s)| {
                            let lfo = 0.5 + 0.5 * det_sin_cycles(step * i as f64);
                            to_f32(f64::from(s) * (1.0 - depth + depth * lfo))
                        })
                        .collect()
                }
                LoopNode::Filter {
                    input: id,
                    mode,
                    cutoff_hz,
                    q,
                } => {
                    let coeffs = BiquadCoeffs::new(*mode, f64::from(*cutoff_hz), f64::from(*q), sample_rate);
                    let mut filter = Biquad::new(coeffs);
                    lookup(&index, &buffers, id)
                        .iter()
                        .map(|&s| to_f32(filter.process(f64::from(s))))
                        .collect()
                }
                LoopNode::OnePole {
                    input: id,
                    mode,
                    cutoff_hz,
                } => {
                    let a = 1.0 - det_exp(-TAU * f64::from(*cutoff_hz) / sample_rate);
                    let mut y = 0.0f64;
                    let high = *mode == OnePoleMode::HighPass;
                    lookup(&index, &buffers, id)
                        .iter()
                        .map(|&s| {
                            let s = f64::from(s);
                            y += a * (s - y);
                            to_f32(if high { s - y } else { y })
                        })
                        .collect()
                }
                LoopNode::Gain { input: id, db } => {
                    let gain = db_to_linear(f64::from(*db));
                    lookup(&index, &buffers, id)
                        .iter()
                        .map(|&s| to_f32(f64::from(s) * gain))
                        .collect()
                }
                LoopNode::Mix { inputs } => {
                    let mut acc = vec![0.0f64; total];
                    for m in inputs {
                        let gain = f64::from(m.gain);
                        for (a, &s) in acc.iter_mut().zip(lookup(&index, &buffers, &m.input)) {
                            *a += f64::from(s) * gain;
                        }
                    }
                    acc.into_iter().map(to_f32).collect()
                }
            };
            // Free input buffers whose last consumer just ran.
            for (_, target) in self.nodes[node].node.inputs() {
                if let Some(&j) = index.get(target)
                    && last_use[j] == pos
                {
                    buffers[j] = Vec::new();
                }
            }
            buffers[node] = out;
        }
        order
            .last()
            .map_or_else(Vec::new, |&last| std::mem::take(&mut buffers[last]))
    }
}

/// Normalised correlation of the loop head and its continuation past the end, clamped to
/// −0.5..1. Only `+ * / sqrt` in a fixed order, so bit-identical on every platform. Near −1 the
/// two halves cancel in the middle of the wrap whatever the gains, so the boost is bounded at
/// +3 dB (the −0.5 floor) rather than chasing the cancellation; silence gives 0.
fn wrap_correlation(head: &[f32], continuation: &[f32]) -> f64 {
    let (mut hc, mut hh, mut cc) = (0.0f64, 0.0f64, 0.0f64);
    for (&h, &c) in head.iter().zip(continuation) {
        let (h, c) = (f64::from(h), f64::from(c));
        hc += h * c;
        hh += h * h;
        cc += c * c;
    }
    let energy = (hh * cc).sqrt();
    if energy <= 1e-30 {
        return 0.0;
    }
    (hc / energy).clamp(-0.5, 1.0)
}

fn check_node(node: &LoopNode, base: &str, nyquist: f32) -> Result<(), LoopGenError> {
    let gain = |name: &str, v: f32| check_range(&format!("{base}.{name}"), v, -16.0, 16.0);
    let freq = |name: &str, v: f32| check_range(&format!("{base}.{name}"), v, 1.0, nyquist * 0.98);
    let q = |v: f32| check_range(&format!("{base}.q"), v, 0.1, 20.0);
    match node {
        LoopNode::Noise { gain: g, .. } => gain("gain", *g),
        LoopNode::Osc {
            hz,
            detune_cents,
            gain: g,
            phase,
            ..
        } => {
            check_range(&format!("{base}.detune_cents"), *detune_cents, -2_400.0, 2_400.0)?;
            check_range(&format!("{base}.phase"), *phase, 0.0, 1.0)?;
            gain("gain", *g)?;
            check_range(&format!("{base}.hz"), *hz, 0.01, nyquist * 0.98)?;
            let detuned = f64::from(*hz) * det_exp2(f64::from(*detune_cents) / 1_200.0);
            if detuned > f64::from(nyquist) * 0.98 {
                return Err(bad(format!("{base}.hz"), "detuned frequency is above Nyquist"));
            }
            Ok(())
        }
        LoopNode::Rumble {
            cutoff_hz,
            q: qv,
            gain: g,
        } => {
            freq("cutoff_hz", *cutoff_hz)?;
            q(*qv)?;
            gain("gain", *g)
        }
        LoopNode::Am { rate_hz, depth, .. } => {
            check_range(&format!("{base}.rate_hz"), *rate_hz, 0.0, 200.0)?;
            check_range(&format!("{base}.depth"), *depth, 0.0, 1.0)
        }
        LoopNode::Filter { cutoff_hz, q: qv, .. } => {
            freq("cutoff_hz", *cutoff_hz)?;
            q(*qv)
        }
        LoopNode::OnePole { cutoff_hz, .. } => freq("cutoff_hz", *cutoff_hz),
        LoopNode::Gain { db, .. } => check_range(&format!("{base}.db"), *db, -96.0, 48.0),
        LoopNode::Mix { inputs } => {
            if inputs.is_empty() {
                return Err(bad(format!("{base}.inputs"), "needs at least one input"));
            }
            for (i, m) in inputs.iter().enumerate() {
                check_range(&format!("{base}.inputs[{i}].gain"), m.gain, -16.0, 16.0)?;
            }
            Ok(())
        }
    }
}

fn lookup<'a>(index: &HashMap<&str, usize>, buffers: &'a [Vec<f32>], id: &str) -> &'a [f32] {
    index.get(id).map_or(&[][..], |&j| &buffers[j][..])
}

fn render_noise(color: NoiseColor, gain: f64, total: usize, rng: &mut Rng) -> Vec<f32> {
    match color {
        NoiseColor::White => (0..total).map(|_| to_f32(white(rng) * gain)).collect(),
        NoiseColor::Pink => {
            let mut state = PinkState::default();
            (0..total)
                .map(|_| to_f32(state.process(white(rng)) * gain))
                .collect()
        }
        NoiseColor::Brown => {
            let mut last = 0.0f64;
            (0..total)
                .map(|_| {
                    last = (last + 0.02 * white(rng)) / 1.02;
                    to_f32(last * 3.5 * gain)
                })
                .collect()
        }
    }
}

// ------------------------------------------------------------------------------------ cache

/// One cached loop, as listed by [`LoopGenCache::entries`].
#[derive(Clone, Debug, PartialEq)]
pub struct LoopGenInfo {
    /// Name of the definition.
    pub name: String,
    /// FNV-1a 64 hash of the file text (the cache key).
    pub content_hash: u64,
    /// Measurements of the rendered loop.
    pub stats: LoopStats,
}

type CacheMap = HashMap<(u64, usize), Arc<RenderedLoop>>;

/// In-memory cache of rendered loops, keyed by the content hash of the `.loopgen.ron` text.
///
/// There is no disk cache in v1 (ADR-299): rendering a few seconds of audio takes milliseconds,
/// the result is a pure function of the file (so a stale entry cannot exist), and a disk cache
/// would add an invalidation and cleanup problem for no measurable gain.
#[derive(Default)]
pub struct LoopGenCache {
    entries: Mutex<CacheMap>,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl LoopGenCache {
    /// The cache key of a file's text.
    pub fn content_hash(text: &str) -> u64 {
        fnv1a(text.bytes())
    }

    /// Renders `text` (a `.loopgen.ron`), or returns the cached loop for identical text.
    pub fn render_text(&self, text: &str) -> Result<Arc<RenderedLoop>, LoopGenError> {
        let key = (Self::content_hash(text), text.len());
        if let Some(found) = self.lock().get(&key) {
            self.hits.fetch_add(1, Ordering::Relaxed);
            return Ok(Arc::clone(found));
        }
        let _span = tracing::info_span!("loopgen_render").entered();
        let rendered = Arc::new(LoopGenDef::parse(text)?.render()?);
        self.misses.fetch_add(1, Ordering::Relaxed);
        self.lock().insert(key, Arc::clone(&rendered));
        Ok(rendered)
    }

    /// Renders `text` and wraps the loop as a [`SoundAsset`].
    pub fn sound_asset(&self, text: &str) -> Result<SoundAsset, AudioError> {
        Ok(self.render_text(text)?.to_sound_asset())
    }

    /// Lookups answered from the cache.
    pub fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    /// Lookups that rendered.
    pub fn misses(&self) -> u64 {
        self.misses.load(Ordering::Relaxed)
    }

    /// Number of cached loops.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// True when nothing is cached.
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    /// The cached loops, sorted by name then hash.
    pub fn entries(&self) -> Vec<LoopGenInfo> {
        let sorted: BTreeMap<(String, u64, usize), LoopGenInfo> = self
            .lock()
            .iter()
            .map(|(&(hash, len), rendered)| {
                (
                    (rendered.name.clone(), hash, len),
                    LoopGenInfo {
                        name: rendered.name.clone(),
                        content_hash: hash,
                        stats: rendered.stats(),
                    },
                )
            })
            .collect();
        sorted.into_values().collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, CacheMap> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(nodes: Vec<LoopNodeDef>, output: &str) -> LoopGenDef {
        LoopGenDef {
            version: 1,
            name: "t".into(),
            sample_rate: 48_000,
            length_secs: 1.0,
            channels: 1,
            seed: 7,
            crossfade_secs: 0.1,
            output_gain_db: 0.0,
            nodes,
            output: output.into(),
        }
    }

    fn node(id: &str, node: LoopNode) -> LoopNodeDef {
        LoopNodeDef { id: id.into(), node }
    }

    fn noise(color: NoiseColor) -> LoopNode {
        LoopNode::Noise { color, gain: 0.5 }
    }

    fn osc(shape: OscShape, hz: f32) -> LoopNode {
        LoopNode::Osc {
            shape,
            hz,
            detune_cents: 0.0,
            gain: 0.5,
            phase: 0.0,
        }
    }

    fn single(n: LoopNode) -> LoopGenDef {
        def(vec![node("a", n)], "a")
    }

    #[test]
    fn deterministic_sine_and_exp_match_libm_closely() {
        for i in 0..=400 {
            let c = f64::from(i) / 100.0 - 2.0;
            assert!((det_sin_cycles(c) - (c * TAU).sin()).abs() < 2e-7, "sin {c}");
        }
        for i in -40..=40 {
            let x = f64::from(i) / 4.0;
            let want = x.exp2();
            assert!(((det_exp2(x) - want) / want).abs() < 1e-10, "exp2 {x}");
        }
        assert!((db_to_linear(-6.0) - 0.501_187).abs() < 1e-5);
    }

    #[test]
    fn same_file_same_samples_different_seed_differs() {
        let a = single(noise(NoiseColor::Pink)).render().expect("renders");
        let b = single(noise(NoiseColor::Pink)).render().expect("renders");
        assert_eq!(a.stats().hash, b.stats().hash);
        assert_eq!(a, b);
        let mut other = single(noise(NoiseColor::Pink));
        other.seed = 8;
        let c = other.render().expect("renders");
        assert_ne!(a.stats().hash, c.stats().hash);
    }

    #[test]
    fn stereo_channels_draw_independent_noise() {
        let mut d = single(noise(NoiseColor::White));
        d.channels = 2;
        let r = d.render().expect("renders");
        assert_eq!(r.samples.len(), 2 * 48_000);
        let differ = r.samples.chunks(2).filter(|f| f[0] != f[1]).count();
        assert!(differ > 40_000, "{differ}");
    }

    #[test]
    fn every_generator_loops_without_a_seam() {
        let sources = [
            noise(NoiseColor::White),
            noise(NoiseColor::Pink),
            noise(NoiseColor::Brown),
            osc(OscShape::Sine, 110.3),
            osc(OscShape::Saw, 55.7),
            osc(OscShape::Square, 80.1),
            osc(OscShape::Triangle, 220.9),
            LoopNode::Rumble {
                cutoff_hz: 90.0,
                q: 0.9,
                gain: 0.5,
            },
        ];
        for source in sources {
            let label = source.variant();
            let r = single(source).render().expect("renders");
            let s = r.stats();
            assert!(s.rms > 0.01, "{label}: audible ({})", s.rms);
            // The documented bound: the wrap step is within 10 % (plus 2e-4) of the largest step inside.
            assert!(
                s.seam_jump <= s.max_step * 1.1 + 2e-4,
                "{label}: seam {} > max step {}",
                s.seam_jump,
                s.max_step
            );
        }
    }

    #[test]
    fn no_crossfade_leaves_a_seam_that_the_crossfade_removes() {
        // 100.5 cycles in 1 s: the loop ends half a cycle off.
        let mut hard = single(osc(OscShape::Sine, 100.5));
        hard.crossfade_secs = 0.0;
        let hard = hard.render().expect("renders").stats();
        let soft = single(osc(OscShape::Sine, 100.5))
            .render()
            .expect("renders")
            .stats();
        assert!(hard.seam_jump > 5.0 * hard.max_step, "{hard:?}");
        assert!(soft.seam_jump <= soft.max_step * 1.1 + 2e-4, "{soft:?}");
    }

    /// RMS gain of a biquad on a sine of `hz`, measured over the last half second.
    fn measured_gain(mode: FilterMode, cutoff: f64, q: f64, hz: f64) -> f64 {
        let mut f = Biquad::new(BiquadCoeffs::new(mode, cutoff, q, 48_000.0));
        let (mut in_sq, mut out_sq) = (0.0, 0.0);
        for i in 0..48_000 {
            let x = (hz / 48_000.0 * f64::from(i) * TAU).sin();
            let y = f.process(x);
            if i >= 24_000 {
                in_sq += x * x;
                out_sq += y * y;
            }
        }
        (out_sq / in_sq).sqrt()
    }

    /// The analog prototype's magnitude at the pre-warped ratio `r` (RBJ biquads are bilinear).
    fn expected_gain(mode: FilterMode, cutoff: f64, q: f64, hz: f64) -> f64 {
        let r =
            (std::f64::consts::PI * hz / 48_000.0).tan() / (std::f64::consts::PI * cutoff / 48_000.0).tan();
        let den = (1.0 - r * r).powi(2) + r * r / (q * q);
        match mode {
            FilterMode::LowPass => (1.0 / den).sqrt(),
            FilterMode::HighPass => (r.powi(4) / den).sqrt(),
            FilterMode::BandPass => ((r * r / (q * q)) / den).sqrt(),
        }
    }

    #[test]
    fn biquad_frequency_response_matches_the_cookbook() {
        for (mode, cutoff, q) in [
            (FilterMode::LowPass, 1_000.0, 0.707),
            (FilterMode::LowPass, 2_000.0, 2.0),
            (FilterMode::HighPass, 1_000.0, 0.707),
            (FilterMode::BandPass, 1_000.0, 1.0),
        ] {
            for hz in [100.0, 500.0, 1_000.0, 2_000.0, 10_000.0] {
                let got = measured_gain(mode, cutoff, q, hz);
                let want = expected_gain(mode, cutoff, q, hz);
                assert!(
                    (got - want).abs() <= 0.02 * want + 0.002,
                    "{mode:?} fc {cutoff} q {q} at {hz}: got {got}, want {want}"
                );
            }
        }
    }

    #[test]
    fn rendered_filter_node_attenuates_above_the_cutoff() {
        let d = def(
            vec![
                node("src", osc(OscShape::Sine, 8_000.0)),
                node(
                    "lp",
                    LoopNode::Filter {
                        input: "src".into(),
                        mode: FilterMode::LowPass,
                        cutoff_hz: 500.0,
                        q: 0.707,
                    },
                ),
            ],
            "lp",
        );
        let s = d.render().expect("renders").stats();
        // 8 kHz is four octaves above the cutoff: about −48 dB on a 0.5 sine.
        assert!(s.peak < 0.01, "{}", s.peak);
    }

    #[test]
    fn one_pole_response_matches_its_transfer_function() {
        let d = def(
            vec![
                node("src", osc(OscShape::Sine, 1_000.0)),
                node(
                    "lp",
                    LoopNode::OnePole {
                        input: "src".into(),
                        mode: OnePoleMode::LowPass,
                        cutoff_hz: 1_000.0,
                    },
                ),
            ],
            "lp",
        );
        let mut d = d;
        d.crossfade_secs = 0.0;
        let r = d.render().expect("renders");
        let rms = r.stats().rms;
        // Input rms 0.5/√2; |H| = a / |1 − (1−a)e^{−jw}| at w = 2π/48.
        let a = 1.0 - (-TAU * 1_000.0 / 48_000.0).exp();
        let w = TAU * 1_000.0 / 48_000.0;
        let b = 1.0 - a;
        let h = a / (1.0 - 2.0 * b * w.cos() + b * b).sqrt();
        let want = 0.5 / 2f64.sqrt() * h;
        assert!((f64::from(rms) - want).abs() < 0.02 * want, "{rms} vs {want}");
    }

    #[test]
    fn am_with_full_depth_follows_the_lfo() {
        let d = def(
            vec![
                node("src", osc(OscShape::Square, 200.0)),
                node(
                    "am",
                    LoopNode::Am {
                        input: "src".into(),
                        rate_hz: 2.0,
                        depth: 1.0,
                    },
                ),
            ],
            "am",
        );
        let r = d.render().expect("renders");
        // The LFO runs from the start of the warm-up (0.25 s = half a period at 2 Hz), so its
        // minimum, where the envelope is zero, falls 0.125 s into the loop.
        let quiet = r.samples[6_000].abs();
        assert!(quiet < 0.05, "{quiet}");
        assert!(r.stats().peak > 0.4);
    }

    #[test]
    fn gain_and_mix_combine() {
        let d = def(
            vec![
                node("a", osc(OscShape::Square, 100.0)),
                node(
                    "g",
                    LoopNode::Gain {
                        input: "a".into(),
                        db: -6.0206,
                    },
                ),
                node(
                    "m",
                    LoopNode::Mix {
                        inputs: vec![
                            LoopMixInput {
                                input: "g".into(),
                                gain: 1.0,
                            },
                            LoopMixInput {
                                input: "a".into(),
                                gain: 1.0,
                            },
                        ],
                    },
                ),
            ],
            "m",
        );
        // 0.5 + 0.25 = 0.75 on the square's plateau (no crossfade: it would blend coherent copies).
        let mut d = d;
        d.crossfade_secs = 0.0;
        let peak = d.render().expect("renders").stats().peak;
        assert!((peak - 0.75).abs() < 0.01, "{peak}");
    }

    #[test]
    fn errors_name_the_key_path() {
        let bad_cutoff = def(
            vec![
                node("src", noise(NoiseColor::White)),
                node(
                    "f",
                    LoopNode::Filter {
                        input: "src".into(),
                        mode: FilterMode::LowPass,
                        cutoff_hz: 90_000.0,
                        q: 1.0,
                    },
                ),
            ],
            "f",
        );
        let e = bad_cutoff.validate().expect_err("cutoff above Nyquist");
        assert_eq!(e.key_path, "nodes[1].node.Filter.cutoff_hz");

        let unknown = def(
            vec![node(
                "g",
                LoopNode::Gain {
                    input: "nope".into(),
                    db: 0.0,
                },
            )],
            "g",
        );
        assert_eq!(
            unknown.validate().expect_err("unknown input").key_path,
            "nodes[0].node.Gain.input"
        );

        let mix = def(
            vec![
                node("a", noise(NoiseColor::White)),
                node(
                    "m",
                    LoopNode::Mix {
                        inputs: vec![
                            LoopMixInput {
                                input: "a".into(),
                                gain: 1.0,
                            },
                            LoopMixInput {
                                input: "zzz".into(),
                                gain: 1.0,
                            },
                        ],
                    },
                ),
            ],
            "m",
        );
        assert_eq!(
            mix.validate().expect_err("unknown mix input").key_path,
            "nodes[1].node.Mix.inputs[1].input"
        );

        let cycle = def(
            vec![
                node(
                    "a",
                    LoopNode::Gain {
                        input: "b".into(),
                        db: 0.0,
                    },
                ),
                node(
                    "b",
                    LoopNode::Gain {
                        input: "a".into(),
                        db: 0.0,
                    },
                ),
            ],
            "a",
        );
        let e = cycle.validate().expect_err("cycle");
        assert!(e.message.contains("cycle"), "{e}");
        assert!(e.key_path.starts_with("nodes["), "{e}");

        assert_eq!(single(noise(NoiseColor::White)).output, "a");
        let mut missing_output = single(noise(NoiseColor::White));
        missing_output.output = "x".into();
        assert_eq!(missing_output.validate().expect_err("output").key_path, "output");
        let mut long = single(noise(NoiseColor::White));
        long.length_secs = 120.0;
        assert_eq!(long.validate().expect_err("length").key_path, "length_secs");
    }

    #[test]
    fn from_ron_parses_the_documented_shape_and_rejects_newer_versions() {
        let text = r#"(
            version: 1, name: "wind", length_secs: 2.0, seed: 3,
            nodes: [
                (id: "n", node: Noise(color: Pink, gain: 0.8)),
                (id: "lp", node: Filter(input: "n", mode: LowPass, cutoff_hz: 400.0)),
            ],
            output: "lp",
        )"#;
        let def = LoopGenDef::from_ron(text).expect("parses");
        assert_eq!(def.sample_rate, 48_000);
        assert_eq!(def.channels, 1);
        let back = LoopGenDef::from_ron(&ron::to_string(&def).expect("saves")).expect("reloads");
        assert_eq!(back, def);
        let newer = text.replace("version: 1", "version: 2");
        let e = LoopGenDef::parse(&newer).expect_err("newer");
        assert_eq!(e.key_path, "version");
        assert!(e.to_string().contains("2 is newer"), "{e}");
    }

    #[test]
    fn cache_is_keyed_by_content() {
        let text = r#"(version: 1, name: "a", length_secs: 0.5, nodes: [(id: "n", node: Noise(color: White))], output: "n")"#;
        let cache = LoopGenCache::default();
        let first = cache.render_text(text).expect("renders");
        let second = cache.render_text(text).expect("cached");
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!((cache.hits(), cache.misses(), cache.len()), (1, 1, 1));
        let other = text.replace("White", "Pink");
        cache.render_text(&other).expect("renders");
        assert_eq!((cache.hits(), cache.misses(), cache.len()), (1, 2, 2));
        assert_eq!(cache.entries().len(), 2);
    }

    #[test]
    fn wav_bytes_have_a_valid_header() {
        let r = single(noise(NoiseColor::White)).render().expect("renders");
        let wav = r.to_wav_bytes();
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(&wav[8..16], b"WAVEfmt ");
        assert_eq!(wav.len(), 44 + r.samples.len() * 2);
    }

    #[test]
    fn fingerprint_lists_variants_and_fields() {
        let f = loopgen_schema_fingerprint();
        for needle in [
            "crossfade_secs",
            "Pink",
            "Filter.cutoff_hz",
            "BandPass",
            "OnePole",
        ] {
            assert!(f.contains(needle), "{needle} missing from {f}");
        }
    }

    /// RMS of one channel over `range` frames.
    fn region_rms(r: &RenderedLoop, channel: usize, range: std::ops::Range<usize>) -> f64 {
        let channels = r.channels as usize;
        let len = range.len().max(1) as f64;
        let sum: f64 = range
            .map(|i| f64::from(r.samples[i * channels + channel]).powi(2))
            .sum();
        (sum / len).sqrt()
    }

    #[test]
    fn a_tone_that_divides_the_loop_does_not_swell_at_the_wrap() {
        // 100 Hz in 1 s: the continuation is the head again. Plain equal-power gains would sum
        // the two copies to 1.41× (+3 dB) in the middle of the 0.1 s wrap.
        let soft = single(osc(OscShape::Sine, 100.0)).render().expect("renders");
        let mut hard = single(osc(OscShape::Sine, 100.0));
        hard.crossfade_secs = 0.0;
        let hard = hard.render().expect("renders");
        let s = soft.stats();
        assert!(s.peak <= 0.5 + 1e-3, "no swell: peak {}", s.peak);
        let worst = soft
            .samples
            .iter()
            .zip(hard.samples.iter())
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(worst < 1e-3, "the wrap of a periodic tone is the tone: {worst}");
        assert!(s.seam_jump <= s.max_step * 1.1 + 2e-4, "{s:?}");
    }

    #[test]
    fn the_wrap_keeps_the_level_of_noise_and_of_partly_alike_content() {
        // White noise (unrelated halves) and a tone + noise mix (correlation about one half):
        // the middle of the 4800-frame wrap matches the body of the loop within 0.75 dB.
        let mixed = def(
            vec![
                node("tone", osc(OscShape::Sine, 100.0)),
                node("hiss", noise(NoiseColor::White)),
                node(
                    "m",
                    LoopNode::Mix {
                        inputs: vec![
                            LoopMixInput {
                                input: "tone".into(),
                                gain: 0.7,
                            },
                            LoopMixInput {
                                input: "hiss".into(),
                                gain: 0.7,
                            },
                        ],
                    },
                ),
            ],
            "m",
        );
        for (label, d) in [("noise", single(noise(NoiseColor::White))), ("mixed", mixed)] {
            let r = d.render().expect("renders");
            let wrap = region_rms(&r, 0, 1_600..3_200);
            let body = region_rms(&r, 0, 10_000..48_000);
            let db = 20.0 * (wrap / body).log10();
            assert!(db.abs() < 0.75, "{label}: wrap is {db:.2} dB off the body");
        }
    }

    #[test]
    fn an_antiphase_wrap_is_bounded_and_seamless() {
        // 100.5 Hz in 1 s: the continuation is the head inverted (correlation −1). Nothing can
        // fill the cancellation, but the compensation is capped and the seam stays smooth.
        let r = single(osc(OscShape::Sine, 100.5)).render().expect("renders");
        let s = r.stats();
        assert!(s.peak <= 0.5 + 1e-3, "no boost past the source: {}", s.peak);
        assert!(r.samples.iter().all(|v| v.is_finite()));
        assert!(s.seam_jump <= s.max_step * 1.1 + 2e-4, "{s:?}");
    }

    #[test]
    fn wrap_correlation_is_clamped_and_silence_safe() {
        let a: Vec<f32> = (0..64).map(|i| (i as f32 * 0.3).sin()).collect();
        let neg: Vec<f32> = a.iter().map(|v| -v).collect();
        assert!((wrap_correlation(&a, &a) - 1.0).abs() < 1e-12);
        assert_eq!(wrap_correlation(&a, &neg), -0.5);
        assert_eq!(wrap_correlation(&[0.0; 8], &[0.0; 8]), 0.0);
        assert_eq!(wrap_correlation(&[], &[]), 0.0);
    }

    #[test]
    fn rendering_is_independent_of_threads_and_of_the_second_channel() {
        let mut d = def(
            vec![
                node("p", noise(NoiseColor::Pink)),
                node(
                    "r",
                    LoopNode::Rumble {
                        cutoff_hz: 120.0,
                        q: 0.9,
                        gain: 0.4,
                    },
                ),
                node(
                    "m",
                    LoopNode::Mix {
                        inputs: vec![
                            LoopMixInput {
                                input: "p".into(),
                                gain: 0.5,
                            },
                            LoopMixInput {
                                input: "r".into(),
                                gain: 0.5,
                            },
                        ],
                    },
                ),
            ],
            "m",
        );
        let mono = d.render().expect("renders");
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let d = d.clone();
                std::thread::spawn(move || d.render().expect("renders").stats().hash)
            })
            .collect();
        for handle in handles {
            assert_eq!(handle.join().expect("render thread"), mono.stats().hash);
        }
        // Channel 0 of a stereo render is the mono render, bit for bit: each channel draws its
        // own streams and measures its own wrap.
        d.channels = 2;
        let stereo = d.render().expect("renders");
        let left: Vec<u32> = stereo.samples.iter().step_by(2).map(|v| v.to_bits()).collect();
        let mono_bits: Vec<u32> = mono.samples.iter().map(|v| v.to_bits()).collect();
        assert_eq!(left, mono_bits);
    }
}
