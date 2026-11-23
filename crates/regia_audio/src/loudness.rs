// SPDX-License-Identifier: MPL-2.0
//! EBU R128 / ITU-R BS.1770-4 loudness measurement and normalization
//! (`docs/specs/audio-loudness.md`, ADR-1563 g, `docs/provenance.md`).
//!
//! Peak and RMS track a waveform; loudness tracks the ear. [`LoudnessMeter`] applies the
//! BS.1770 **K-weighting** (a +4 dB high shelf, the "head" pre-filter, followed by the "RLB"
//! high-pass), accumulates the channel-weighted mean square, and reports the three standard
//! values — momentary (400 ms), short-term (3 s) and gated integrated loudness — in **LUFS**
//! (Loudness Units relative to Full Scale), plus the **loudness range** (LRA, EBU Tech 3342) and
//! the **true peak** (an oversampled maximum that catches inter-sample peaks). [`normalize_gain`]
//! turns a measured loudness into the gain that brings it to a target, which is what EBU R128
//! normalization means in this engine: bake a per-asset gain at cook time so a quiet voice line
//! and a loud explosion play at a consistent perceived level.
//!
//! # Sources
//!
//! The algorithm is re-derived from the public recommendations (no source code was read): the
//! K-weighting biquad coefficients for 48 kHz, the `−0.691` offset, the `−70 LUFS` absolute gate,
//! the `−10 LU` relative gate and the 400 ms / 3 s windows are the values published in
//! ITU-R BS.1770-4 (Annex 1 and § 4); the LRA is EBU Tech 3342's 10th–95th percentile of the
//! short-term loudness after the absolute gate and the −20 LU relative gate; the true peak follows
//! BS.1770-4 Annex 2. See `docs/provenance.md`.
//!
//! # Sample rate
//!
//! The K-weighting is exact at every rate. The published 48 kHz table is inverted once into its
//! analog prototypes (the shelf and the high-pass, each a center frequency, a Q and three gains),
//! and the bilinear transform with frequency pre-warping (R. Bristow-Johnson, "Audio EQ Cookbook")
//! maps them to the rate of the buffer. At 48 kHz that returns the published table. Window and
//! block sizes are computed from the same rate.
//!
//! # True peak
//!
//! BS.1770-4 Annex 2 raises the sampling rate to at least 192 kHz, low-pass filters and takes the
//! largest magnitude: 4× below 96 kHz with the Annex's 48-tap polyphase filter, 2× at 96 kHz and
//! above ([`true_peak_oversampling`]). The Annex's 12.04 dB head-room attenuation exists for integer
//! arithmetic and is not needed in `f64`. A true-peak reading never falls below the sample peak:
//! the filter's four phases sit between the input samples, so the input samples are checked too.

use std::collections::VecDeque;

use crate::spatial::{db_to_gain, gain_to_db};

/// The EBU R128 reference loudness every normalization is measured against, in LUFS.
pub const REFERENCE_LUFS: f32 = -23.0;

/// The BS.1770 offset: loudness `L = −0.691 + 10·log₁₀(Σ Gᵢ·zᵢ)`, dB.
pub const LOUDNESS_OFFSET_DB: f64 = -0.691;

/// Momentary-loudness window, seconds (BS.1770 § 4.1).
pub const MOMENTARY_SECS: f64 = 0.4;

/// Short-term-loudness window, seconds (BS.1770 § 4.1).
pub const SHORT_TERM_SECS: f64 = 3.0;

/// Gating-block length, seconds (BS.1770 § 4.1: 400 ms with 75 % overlap).
pub const GATING_BLOCK_SECS: f64 = 0.4;

/// Gating-block hop (75 % overlap of [`GATING_BLOCK_SECS`]), seconds.
pub const GATING_HOP_SECS: f64 = 0.1;

/// Absolute gate: gating blocks below this loudness are excluded, LUFS (BS.1770 § 4.1.1).
pub const ABSOLUTE_GATE_LUFS: f32 = -70.0;

/// Relative gate: blocks more than this many LU below the gated mean are excluded (BS.1770 § 4.1.2).
pub const RELATIVE_GATE_LU: f32 = -10.0;

/// The largest normalization gain applied in either direction, dB. A larger correction (e.g.
/// normalizing near-silence) is clamped so a measurement mistake cannot produce an enormous gain.
pub const MAX_NORMALIZE_GAIN_DB: f32 = 60.0;

/// Errors from [`LoudnessMeter::new`] and [`measure_loudness`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoudnessError {
    /// The buffer has no samples (nothing to measure).
    Empty,
    /// The sample rate is zero, which no window size can be derived from.
    ZeroSampleRate,
    /// Only mono (1), stereo (2) and 5.1 (6) interleaving are supported.
    UnsupportedChannels(u8),
    /// The sample rate is above [`crate::MAX_SOUND_SAMPLE_RATE`]; the meter's windows are sized
    /// from it, so it is refused before anything is allocated.
    SampleRateTooHigh(u32),
}

impl std::fmt::Display for LoudnessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => f.write_str("no samples to measure"),
            Self::ZeroSampleRate => f.write_str("sample rate is zero"),
            Self::UnsupportedChannels(channels) => {
                write!(f, "unsupported channel count {channels} (use 1, 2 or 6 for 5.1)")
            }
            Self::SampleRateTooHigh(rate) => write!(
                f,
                "sample rate {rate} Hz is above the {} Hz maximum",
                crate::MAX_SOUND_SAMPLE_RATE
            ),
        }
    }
}

impl std::error::Error for LoudnessError {}

/// The result of measuring a buffer or running a meter to completion.
///
/// Loudness values are in **LUFS** (negative for anything quieter than full scale); the loudness
/// range is in **LU** and the true peak in **dBTP** (decibels true peak, 0 = full scale).
/// A value that has no data behind it is `f32::NEG_INFINITY` (an integrated loudness with no
/// complete gating block, or a momentary value with no samples), and the loudness range is `0.0`
/// when fewer than two short-term readings exist.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Loudness {
    /// Loudness over the last 400 ms.
    pub momentary_lufs: f32,
    /// Loudness over the last 3 s.
    pub short_term_lufs: f32,
    /// Gated loudness over the whole buffer (absolute + relative gate).
    pub integrated_lufs: f32,
    /// Loudness range (10th–95th percentile of short-term loudness), LU.
    pub loudness_range_lu: f32,
    /// True peak (BS.1770-4 Annex 2, [`true_peak_oversampling`]), dBTP.
    pub true_peak_db: f32,
}

/// A second-order biquad in transposed direct form II, with `f64` state for the long loudness
/// accumulations (the same structure as `regia_audio::path::Biquad`).
#[derive(Clone, Copy, Debug)]
struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    z1: f64,
    z2: f64,
}

impl Biquad {
    #[inline]
    fn process(&mut self, x: f64) -> f64 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }
}

/// One K-weighting stage as a normalized analog prototype:
/// `H(s) = (vh·s² + (vb/q)·s + vl) / (s² + s/q + 1)`, with `s` in units of `2π·f0`.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Prototype {
    f0_hz: f64,
    q: f64,
    vh: f64,
    vb: f64,
    vl: f64,
}

impl Prototype {
    /// `K` never reaches the pole of `tan` at the rate where `f0` meets Nyquist: a rate too low
    /// for the stage still gets a stable filter, with the corner just under Nyquist.
    const MAX_WARP_RADIANS: f64 = 0.49 * std::f64::consts::PI;

    /// The prototype whose bilinear transform at `rate` is the biquad `[b0, b1, b2, a1, a2]`.
    /// The five digital coefficients and the five prototype values map one to one.
    fn from_digital(c: &[f64; 5], rate: f64) -> Self {
        let [b0, b1, b2, a1, a2] = *c;
        let at_dc = 1.0 + a1 + a2;
        let at_nyquist = 1.0 - a1 + a2;
        let k = (at_dc / at_nyquist).sqrt();
        let k_over_q = 2.0 * (1.0 - a2) / at_nyquist;
        Self {
            f0_hz: k.atan() * rate / std::f64::consts::PI,
            q: k / k_over_q,
            vh: (b0 - b1 + b2) / at_nyquist,
            vb: (b0 - b2) / (1.0 - a2),
            vl: (b0 + b1 + b2) / at_dc,
        }
    }

    /// The bilinear transform with `f0` pre-warped: `s = (z − 1) / (K·(z + 1))`,
    /// `K = tan(π·f0 / rate)`. Returns `[b0, b1, b2, a1, a2]`.
    fn bilinear(&self, rate: f64) -> [f64; 5] {
        let k = (std::f64::consts::PI * self.f0_hz / rate)
            .min(Self::MAX_WARP_RADIANS)
            .tan();
        let k_over_q = k / self.q;
        let k2 = k * k;
        let a0 = 1.0 + k_over_q + k2;
        [
            (self.vh + self.vb * k_over_q + self.vl * k2) / a0,
            2.0 * (self.vl * k2 - self.vh) / a0,
            (self.vh - self.vb * k_over_q + self.vl * k2) / a0,
            2.0 * (k2 - 1.0) / a0,
            (1.0 - k_over_q + k2) / a0,
        ]
    }
}

/// The rate of the published K-weighting table, Hz.
const TABLE_RATE: f64 = 48_000.0;

/// The BS.1770 K-weighting: the "pre-filter" (+4 dB high shelf) followed by the "RLB"
/// high-pass, at any sample rate.
#[derive(Clone, Debug)]
pub(crate) struct KWeight {
    pre: Biquad,
    rlb: Biquad,
}

impl KWeight {
    /// BS.1770-4 Table (48 kHz), stage 1: the pre-filter (a +4 dB high shelf).
    const PRE: [f64; 5] = [
        1.535_124_859_59,
        -2.691_696_189_41,
        1.198_392_810_85,
        -1.690_659_293_18,
        0.732_480_774_216,
    ];

    /// BS.1770-4 Table (48 kHz), stage 2: the RLB high-pass.
    const RLB: [f64; 5] = [1.0, -2.0, 1.0, -1.990_047_454_83, 0.990_072_250_366];

    /// The two stages' `[b0, b1, b2, a1, a2]` at `rate`, Hz.
    fn coefficients(rate: f64) -> ([f64; 5], [f64; 5]) {
        (
            Prototype::from_digital(&Self::PRE, TABLE_RATE).bilinear(rate),
            Prototype::from_digital(&Self::RLB, TABLE_RATE).bilinear(rate),
        )
    }

    pub(crate) fn new(rate: f64) -> Self {
        let biquad = |c: &[f64; 5]| Biquad {
            b0: c[0],
            b1: c[1],
            b2: c[2],
            a1: c[3],
            a2: c[4],
            z1: 0.0,
            z2: 0.0,
        };
        let (pre, rlb) = Self::coefficients(rate);
        Self {
            pre: biquad(&pre),
            rlb: biquad(&rlb),
        }
    }

    /// Filters one sample. A non-finite sample is dropped and the state cleared, so later finite
    /// samples recover (matching `path::Biquad`).
    #[inline]
    pub(crate) fn process(&mut self, x: f32) -> f64 {
        if !x.is_finite() {
            self.pre.z1 = 0.0;
            self.pre.z2 = 0.0;
            self.rlb.z1 = 0.0;
            self.rlb.z2 = 0.0;
            return 0.0;
        }
        self.rlb.process(self.pre.process(f64::from(x)))
    }
}

/// The BS.1770 K-weighting biquad coefficients — `[b0, b1, b2, a1, a2]` of the pre-filter
/// (a +4 dB high shelf) and of the RLB high-pass — at `rate`, Hz. Shared with meters that
/// detect their rate at runtime (the bus meter tap) so every meter in the crate weights
/// alike.
pub(crate) fn k_weight_coefficients(rate: f64) -> ([f64; 5], [f64; 5]) {
    KWeight::coefficients(rate)
}

/// A fixed-capacity sliding window of per-sample power with an `O(1)` running sum.
#[derive(Debug)]
struct RingPower {
    buf: Vec<f64>,
    pos: usize,
    len: usize,
    sum: f64,
}

impl RingPower {
    fn new(capacity: usize) -> Self {
        Self {
            buf: vec![0.0; capacity],
            pos: 0,
            len: 0,
            sum: 0.0,
        }
    }

    #[inline]
    fn is_full(&self) -> bool {
        self.len == self.buf.len()
    }

    #[inline]
    fn push(&mut self, x: f64) {
        if self.len == self.buf.len() {
            self.sum -= self.buf[self.pos];
        } else {
            self.len += 1;
        }
        self.buf[self.pos] = x;
        self.sum += x;
        self.pos = (self.pos + 1) % self.buf.len();
    }

    #[inline]
    fn mean(&self) -> f64 {
        if self.len == 0 {
            0.0
        } else {
            self.sum / self.len as f64
        }
    }
}

/// Taps per phase of the true-peak interpolators.
const TRUE_PEAK_TAPS: usize = 12;

/// BS.1770-4 Annex 2: the 48-tap polyphase low-pass for 4× oversampling, in its four phases of 12
/// taps. Its passband holds ±0.11 dB to 20 kHz at 48 kHz, and it reaches −39.8 dB by 28 kHz.
const ANNEX_2_PHASES: [[f64; TRUE_PEAK_TAPS]; 4] = [
    [
        0.001_708_984_375,
        0.010_986_328_125,
        -0.019_653_320_312_5,
        0.033_203_125,
        -0.059_448_242_187_5,
        0.137_329_101_562_5,
        0.972_167_968_75,
        -0.102_294_921_875,
        0.047_607_421_875,
        -0.026_611_328_125,
        0.014_892_578_125,
        -0.008_300_781_25,
    ],
    [
        -0.029_174_804_687_5,
        0.029_296_875,
        -0.051_757_812_5,
        0.089_111_328_125,
        -0.166_503_906_25,
        0.465_087_890_625,
        0.779_785_156_25,
        -0.200_317_382_812_5,
        0.101_562_5,
        -0.058_227_539_062_5,
        0.033_081_054_687_5,
        -0.018_920_898_437_5,
    ],
    [
        -0.018_920_898_437_5,
        0.033_081_054_687_5,
        -0.058_227_539_062_5,
        0.101_562_5,
        -0.200_317_382_812_5,
        0.779_785_156_25,
        0.465_087_890_625,
        -0.166_503_906_25,
        0.089_111_328_125,
        -0.051_757_812_5,
        0.029_296_875,
        -0.029_174_804_687_5,
    ],
    [
        -0.008_300_781_25,
        0.014_892_578_125,
        -0.026_611_328_125,
        0.047_607_421_875,
        -0.102_294_921_875,
        0.972_167_968_75,
        0.137_329_101_562_5,
        -0.059_448_242_187_5,
        0.033_203_125,
        -0.019_653_320_312_5,
        0.010_986_328_125,
        0.001_708_984_375,
    ],
];

/// The rate from which 2× oversampling already reaches the 192 kHz Annex 2 asks for.
const TRUE_PEAK_2X_RATE: u32 = 96_000;

/// The oversampling factor of the true-peak meter at `sample_rate` (BS.1770-4 Annex 2): 4× below
/// 96 kHz, 2× at 96 kHz and above.
///
/// ```
/// # use regia_audio::true_peak_oversampling;
/// assert_eq!(true_peak_oversampling(48_000), 4);
/// assert_eq!(true_peak_oversampling(96_000), 2);
/// ```
#[must_use]
pub const fn true_peak_oversampling(sample_rate: u32) -> u32 {
    if sample_rate >= TRUE_PEAK_2X_RATE { 2 } else { 4 }
}

/// The half-sample interpolator of the 2× path. The Annex prints no 2× table, so this is a 12-tap
/// Hann-windowed sinc, normalized to unity at DC. Its other phase is the input sample itself.
fn half_sample_phase() -> [f64; TRUE_PEAK_TAPS] {
    let mut row = [0.0f64; TRUE_PEAK_TAPS];
    let centre = (TRUE_PEAK_TAPS as f64 - 1.0) / 2.0;
    for (tap, coeff) in row.iter_mut().enumerate() {
        let t = centre - tap as f64;
        let pi_t = std::f64::consts::PI * t;
        let window = 0.5 * (1.0 + (pi_t / (TRUE_PEAK_TAPS as f64 / 2.0)).cos());
        *coeff = pi_t.sin() / pi_t * window;
    }
    let sum: f64 = row.iter().sum();
    row.map(|coeff| coeff / sum)
}

/// A true-peak oversampler for one channel: polyphase FIR over the last [`TRUE_PEAK_TAPS`] input
/// samples. The stream is taken as zero before its first sample and after its last (the history
/// starts silent, and [`LoudnessMeter::finish`] flushes [`TRUE_PEAK_LEAD`] more), so the pairs at
/// both ends are measured too.
#[derive(Debug, Clone)]
pub(crate) struct TruePeak {
    phases: Vec<[f64; TRUE_PEAK_TAPS]>,
    /// The history written twice, so the newest [`TRUE_PEAK_TAPS`] samples are always one slice.
    history: [f64; TRUE_PEAK_TAPS * 2],
    newest: usize,
}

impl TruePeak {
    pub(crate) fn new(sample_rate: u32) -> Self {
        Self {
            phases: if true_peak_oversampling(sample_rate) == 2 {
                vec![half_sample_phase()]
            } else {
                ANNEX_2_PHASES.to_vec()
            },
            history: [0.0; TRUE_PEAK_TAPS * 2],
            newest: 0,
        }
    }

    /// The peak of the points still to come if the stream ended now: the last sample pairs
    /// against six zeros of silence after it.
    fn tail_peak(&self) -> f64 {
        let mut tail = self.clone();
        (0..TRUE_PEAK_LEAD).map(|_| tail.push(0.0)).fold(0.0, f64::max)
    }

    /// Forgets the history (a new measurement); allocation-free.
    pub(crate) fn clear(&mut self) {
        self.history = [0.0; TRUE_PEAK_TAPS * 2];
        self.newest = 0;
    }

    /// Feeds one sample and returns the largest magnitude seen in this step: the sample itself
    /// plus the interpolated points that become available with it.
    #[inline]
    pub(crate) fn push(&mut self, x: f64) -> f64 {
        self.newest = self.newest.checked_sub(1).unwrap_or(TRUE_PEAK_TAPS - 1);
        self.history[self.newest] = x;
        self.history[self.newest + TRUE_PEAK_TAPS] = x;
        let window = &self.history[self.newest..self.newest + TRUE_PEAK_TAPS];
        let mut peak = x.abs();
        for phase in &self.phases {
            let y: f64 = phase
                .iter()
                .zip(window)
                .map(|(coeff, sample)| coeff * sample)
                .sum();
            peak = peak.max(y.abs());
        }
        peak
    }
}

/// Zeros the true-peak interpolator sees after the stream (half its 12 taps).
const TRUE_PEAK_LEAD: usize = TRUE_PEAK_TAPS / 2;

/// Loudness in LUFS from a channel-weighted mean square of K-weighted samples.
#[inline]
pub(crate) fn lufs_from_power(power: f64) -> f32 {
    if power <= 0.0 {
        f32::NEG_INFINITY
    } else {
        (LOUDNESS_OFFSET_DB + 10.0 * power.log10()) as f32
    }
}

/// BS.1770-4 Table 3 channel weights in the 5.1 order L, R, C, LFE, Ls, Rs: the front channels
/// `1.0`, the surrounds `1.41` (+1.5 dB), the LFE excluded (`0.0`).
pub const FIVE_ONE_WEIGHTS: [f32; 6] = [1.0, 1.0, 1.0, 0.0, 1.41, 1.41];

/// The BS.1770-4 channel weights `Gᵢ` for a layout of `channels` interleaved channels: mono `[1.0]`,
/// stereo `[1.0, 1.0]`, 5.1 [`FIVE_ONE_WEIGHTS`] (L, R, C, LFE, Ls, Rs); `None` for any other count.
///
/// ```
/// assert_eq!(regia_audio::channel_weights(6), Some(&[1.0, 1.0, 1.0, 0.0, 1.41, 1.41][..]));
/// assert_eq!(regia_audio::channel_weights(4), None);
/// ```
#[must_use]
pub fn channel_weights(channels: u8) -> Option<&'static [f32]> {
    match channels {
        1 => Some(&[1.0]),
        2 => Some(&[1.0, 1.0]),
        6 => Some(&FIVE_ONE_WEIGHTS),
        _ => None,
    }
}

/// A streaming EBU R128 loudness meter.
///
/// Feed it interleaved samples with [`LoudnessMeter::push`]; read the live momentary, short-term
/// and true-peak values at any time, and [`LoudnessMeter::finish`] for the final gated integrated
/// loudness and loudness range. Feed the whole buffer at once and it is an offline meter.
#[derive(Debug)]
pub struct LoudnessMeter {
    channels: u8,
    weights: Vec<f64>,
    filters: Vec<KWeight>,
    momentary: RingPower,
    short_term: RingPower,
    hop_len: usize,
    block_hops: usize,
    hop_remaining: usize,
    hop_energy: f64,
    block_hops_buf: VecDeque<f64>,
    blocks: Vec<f64>,
    true_peaks: Vec<TruePeak>,
    peak: f64,
    /// The short-term readings for the loudness range.
    lra: LraHistogram,
}

/// The loudness-range histogram (EBU Tech 3342): short-term readings in 0.1 LU bins over
/// `LRA_MIN_LUFS ..= LRA_MIN_LUFS + LRA_SPAN_LU`, absolute-gated at −70 LUFS. Allocated once; recording
/// and reading never allocate (the live program meter runs it on the audio thread).
#[derive(Clone, Debug)]
pub(crate) struct LraHistogram {
    bins: Vec<u64>,
    /// Sum of the mean-square powers of the readings in `bins`, for the LRA's relative gate (EBU
    /// Tech 3342: −20 LU below the power mean of the absolute-gated readings).
    power_sum: f64,
}

impl LraHistogram {
    pub(crate) fn new() -> Self {
        Self {
            bins: vec![0; LRA_BINS],
            power_sum: 0.0,
        }
    }

    /// Forgets every reading.
    pub(crate) fn clear(&mut self) {
        self.bins.fill(0);
        self.power_sum = 0.0;
    }

    /// Adds one short-term reading (its mean-square `power`), absolute-gated at −70 LUFS.
    pub(crate) fn record(&mut self, power: f64) {
        let lufs = lufs_from_power(power);
        if !lufs.is_finite() || lufs < ABSOLUTE_GATE_LUFS {
            return;
        }
        let idx = ((lufs - LRA_MIN_LUFS) / LRA_STEP_LU).floor() as isize;
        if idx < 0 {
            return;
        }
        if let Some(bin) = self.bins.get_mut(idx as usize) {
            *bin += 1;
            self.power_sum += power;
        }
    }

    /// The readings that pass the −70 LUFS absolute gate and the −20 LU relative gate (below the
    /// power mean of the absolute-gated readings), 95th minus 10th percentile, LU; `0.0` with fewer
    /// than two. The relative gate is applied at the histogram's 0.1 LU resolution (the bin holding
    /// the threshold is kept).
    pub(crate) fn range(&self) -> f32 {
        let absolute: u64 = self.bins.iter().sum();
        if absolute < 2 {
            return 0.0;
        }
        let mean_power = self.power_sum / absolute as f64;
        let threshold = lufs_from_power(mean_power) + LRA_RELATIVE_GATE_LU;
        let first = if threshold.is_finite() {
            ((threshold - LRA_MIN_LUFS) / LRA_STEP_LU).floor().max(0.0) as usize
        } else {
            0
        };
        let first = first.min(self.bins.len());
        let total: u64 = self.bins[first..].iter().sum();
        if total < 2 {
            return 0.0;
        }
        let p10 = self.percentile(first, 0.10, total);
        let p95 = self.percentile(first, 0.95, total);
        (p95 - p10).max(0.0)
    }

    /// The histogram value below which `frac` of the readings fall, with linear interpolation
    /// inside the containing bin.
    fn percentile(&self, first: usize, frac: f64, total: u64) -> f32 {
        let target = frac * total as f64;
        let mut cumulative = 0u64;
        for (i, &count) in self.bins.iter().enumerate().skip(first) {
            if count == 0 {
                continue;
            }
            let next = cumulative + count;
            if next as f64 >= target {
                let in_bin = (target - cumulative as f64) / (count as f64);
                return (f64::from(LRA_MIN_LUFS) + (i as f64 + in_bin) * f64::from(LRA_STEP_LU)) as f32;
            }
            cumulative = next;
        }
        LRA_MIN_LUFS + LRA_SPAN_LU
    }
}

/// The LRA's relative gate: short-term readings more than this many LU below the power mean of
/// the absolute-gated readings are left out (EBU Tech 3342).
const LRA_RELATIVE_GATE_LU: f32 = -20.0;

/// Lowest LRA histogram bin, LUFS (the absolute gate is `−70`, so nothing below matters).
const LRA_MIN_LUFS: f32 = -70.0;

/// LRA histogram span above [`LRA_MIN_LUFS`], LU.
const LRA_SPAN_LU: f32 = 90.0;

/// LRA histogram resolution, LU per bin.
const LRA_STEP_LU: f32 = 0.1;

/// LRA histogram bin count ([`LRA_SPAN_LU`] / [`LRA_STEP_LU`]).
const LRA_BINS: usize = 900;

impl LoudnessMeter {
    /// Builds a meter for `channels` interleaved samples (1 = mono, 2 = stereo, 6 = 5.1 in the
    /// order L, R, C, LFE, Ls, Rs; [`channel_weights`]) at `sample_rate`.
    ///
    /// ```
    /// # use regia_audio::LoudnessMeter;
    /// let mut meter = LoudnessMeter::new(2, 48_000).expect("stereo at 48 kHz is supported");
    /// meter.push(&[0.1, -0.1, 0.2, -0.2]);
    /// let _ = meter.finish();
    /// ```
    pub fn new(channels: u8, sample_rate: u32) -> Result<Self, LoudnessError> {
        if sample_rate == 0 {
            return Err(LoudnessError::ZeroSampleRate);
        }
        if sample_rate > crate::MAX_SOUND_SAMPLE_RATE {
            return Err(LoudnessError::SampleRateTooHigh(sample_rate));
        }
        let Some(weights) = channel_weights(channels) else {
            return Err(LoudnessError::UnsupportedChannels(channels));
        };
        let weights: Vec<f64> = weights.iter().map(|&g| f64::from(g)).collect();
        let rate = f64::from(sample_rate);
        let momentary_len = (MOMENTARY_SECS * rate).round().max(1.0) as usize;
        let short_len = (SHORT_TERM_SECS * rate).round().max(1.0) as usize;
        let hop_len = (GATING_HOP_SECS * rate).round().max(1.0) as usize;
        let block_hops = (GATING_BLOCK_SECS / GATING_HOP_SECS).round().max(1.0) as usize;
        let filters = (0..channels).map(|_| KWeight::new(rate)).collect();
        let true_peaks = (0..channels).map(|_| TruePeak::new(sample_rate)).collect();
        Ok(Self {
            channels,
            weights,
            filters,
            momentary: RingPower::new(momentary_len),
            short_term: RingPower::new(short_len),
            hop_len,
            block_hops,
            hop_remaining: hop_len,
            hop_energy: 0.0,
            block_hops_buf: VecDeque::with_capacity(block_hops),
            blocks: Vec::new(),
            true_peaks,
            peak: 0.0,
            lra: LraHistogram::new(),
        })
    }

    /// The number of channels the meter was built for.
    #[must_use]
    pub fn channels(&self) -> u8 {
        self.channels
    }

    /// Feeds interleaved samples (`channels` per frame; a trailing partial frame is ignored).
    pub fn push(&mut self, interleaved: &[f32]) {
        let _span = tracing::trace_span!(target: "regia_audio", "loudness_push").entered();
        for frame in interleaved.chunks_exact(self.channels as usize) {
            let mut power = 0.0f64;
            for (ch, &sample) in frame.iter().enumerate() {
                let y = self.filters[ch].process(sample);
                power += self.weights[ch] * y * y;
            }
            self.momentary.push(power);
            self.short_term.push(power);
            for (ch, &sample) in frame.iter().enumerate() {
                let x = if sample.is_finite() {
                    f64::from(sample)
                } else {
                    0.0
                };
                self.peak = self.peak.max(self.true_peaks[ch].push(x));
            }
            self.hop_energy += power;
            self.hop_remaining -= 1;
            if self.hop_remaining == 0 {
                self.finish_hop();
                self.hop_remaining = self.hop_len;
                self.hop_energy = 0.0;
            }
        }
    }

    /// Completes one 100 ms hop: forms a 400 ms gating block from the last four hops and samples
    /// the short-term loudness for the LRA histogram.
    fn finish_hop(&mut self) {
        let hop_mean = self.hop_energy / self.hop_len as f64;
        self.block_hops_buf.push_back(hop_mean);
        if self.block_hops_buf.len() == self.block_hops {
            let block_mean = self.block_hops_buf.iter().sum::<f64>() / self.block_hops as f64;
            self.blocks.push(block_mean);
            self.block_hops_buf.pop_front();
        }
        if self.short_term.is_full() {
            self.lra.record(self.short_term.mean());
        }
    }

    /// Live momentary loudness (last 400 ms), LUFS; `−∞` before any sample.
    #[must_use]
    pub fn momentary_lufs(&self) -> f32 {
        lufs_from_power(self.momentary.mean())
    }

    /// Live short-term loudness (last 3 s), LUFS; `−∞` before any sample.
    #[must_use]
    pub fn short_term_lufs(&self) -> f32 {
        lufs_from_power(self.short_term.mean())
    }

    /// Live true peak, dBTP; `−100` before any sample.
    #[must_use]
    pub fn true_peak_db(&self) -> f32 {
        gain_to_db(self.peak as f32)
    }

    /// Gated integrated loudness over all completed gating blocks, LUFS; `−∞` when none exist.
    #[must_use]
    pub fn integrated_lufs(&self) -> f32 {
        let mut kept: Vec<f64> = self
            .blocks
            .iter()
            .copied()
            .filter(|&b| lufs_from_power(b) >= ABSOLUTE_GATE_LUFS)
            .collect();
        if kept.is_empty() {
            return f32::NEG_INFINITY;
        }
        let mean = kept.iter().sum::<f64>() / kept.len() as f64;
        // The relative gate is −10 LU below the (absolute-gated) mean: in the power domain that
        // is 10^(−10/10) = 1/10 of the mean.
        let threshold = mean * 10f64.powf(f64::from(RELATIVE_GATE_LU) / 10.0);
        kept.retain(|&b| b >= threshold);
        if kept.is_empty() {
            return f32::NEG_INFINITY;
        }
        let final_mean = kept.iter().sum::<f64>() / kept.len() as f64;
        lufs_from_power(final_mean)
    }

    /// Loudness range from the short-term histogram (EBU Tech 3342), LU: the readings that pass
    /// the −70 LUFS absolute gate and the −20 LU relative gate (below the power mean of the
    /// absolute-gated readings), 95th minus 10th percentile. The relative gate is applied at the
    /// histogram's 0.1 LU resolution (the bin holding the threshold is kept).
    #[must_use]
    pub fn loudness_range_lu(&self) -> f32 {
        self.lra.range()
    }

    /// The final measurement: momentary, short-term, gated integrated loudness, loudness range
    /// and true peak. The true peak includes the points between the last samples and the silence
    /// after the stream, which the live [`LoudnessMeter::true_peak_db`] has not seen yet.
    #[must_use]
    pub fn finish(&self) -> Loudness {
        let tail = self
            .true_peaks
            .iter()
            .map(TruePeak::tail_peak)
            .fold(0.0, f64::max);
        Loudness {
            momentary_lufs: self.momentary_lufs(),
            short_term_lufs: self.short_term_lufs(),
            integrated_lufs: self.integrated_lufs(),
            loudness_range_lu: self.loudness_range_lu(),
            true_peak_db: gain_to_db(self.peak.max(tail) as f32),
        }
    }
}

/// Measures an interleaved buffer in one call (mono, stereo or 5.1) and returns the full result.
///
/// ```
/// # use regia_audio::measure_loudness;
/// let rate = 48_000u32;
/// // A full-scale 1 kHz sine is loud — well above −10 LUFS.
/// let samples: Vec<f32> = (0..rate)
///     .map(|i| ((i as f32 / rate as f32) * 1000.0 * std::f32::consts::TAU).sin())
///     .collect();
/// let loud = measure_loudness(&samples, 1, rate).expect("valid input");
/// assert!(loud.integrated_lufs > -10.0);
/// ```
pub fn measure_loudness(
    interleaved: &[f32],
    channels: u8,
    sample_rate: u32,
) -> Result<Loudness, LoudnessError> {
    if interleaved.is_empty() {
        return Err(LoudnessError::Empty);
    }
    let mut meter = LoudnessMeter::new(channels, sample_rate)?;
    meter.push(interleaved);
    Ok(meter.finish())
}

/// The gain that brings a signal measured at `measured_lufs` to `target_lufs`: `10^((target −
/// measured)/20)`, clamped to ±[`MAX_NORMALIZE_GAIN_DB`]. A non-finite input returns `1.0`
/// (no change) rather than a NaN that would poison a mixer volume.
///
/// ```
/// # use regia_audio::normalize_gain;
/// // +7 dB: bring −23 LUFS up to −16 LUFS.
/// let gain = normalize_gain(-23.0, -16.0);
/// assert!((gain - 2.2387).abs() < 1e-3);
/// ```
#[must_use]
pub fn normalize_gain(measured_lufs: f32, target_lufs: f32) -> f32 {
    if !measured_lufs.is_finite() || !target_lufs.is_finite() {
        return 1.0;
    }
    let db = (target_lufs - measured_lufs).clamp(-MAX_NORMALIZE_GAIN_DB, MAX_NORMALIZE_GAIN_DB);
    db_to_gain(db)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The BS.1770 48 kHz K-weighting coefficients, repeated as literals so the analytical
    /// response check is independent of the constants in the filter itself.
    const PRE: [f64; 5] = [
        1.535_124_859_59,
        -2.691_696_189_41,
        1.198_392_810_85,
        -1.690_659_293_18,
        0.732_480_774_216,
    ];
    const RLB: [f64; 5] = [1.0, -2.0, 1.0, -1.990_047_454_83, 0.990_072_250_366];

    /// `|H(e^jω)|²` of a biquad at radian frequency `w`.
    fn biquad_gain2(c: &[f64; 5], w: f64) -> f64 {
        let (sin_w, cos_w) = w.sin_cos();
        let (sin_2w, cos_2w) = (2.0 * w).sin_cos();
        let n_re = c[0] + c[1] * cos_w + c[2] * cos_2w;
        let n_im = -(c[1] * sin_w + c[2] * sin_2w);
        let d_re = 1.0 + c[3] * cos_w + c[4] * cos_2w;
        let d_im = -(c[3] * sin_w + c[4] * sin_2w);
        (n_re * n_re + n_im * n_im) / (d_re * d_re + d_im * d_im)
    }

    /// A mono sine of `seconds` at `hz` and amplitude `amp`, 48 kHz.
    fn sine(hz: f32, seconds: f64, amp: f32) -> Vec<f32> {
        let len = (seconds * 48_000.0) as usize;
        (0..len)
            .map(|i| (i as f32 / 48_000.0 * hz * std::f32::consts::TAU).sin() * amp)
            .collect()
    }

    /// DC and Nyquist gains of a biquad from its coefficients (z = ±1).
    fn dc_gain(c: &[f64; 5]) -> f64 {
        (c[0] + c[1] + c[2]) / (1.0 + c[3] + c[4])
    }

    fn nyquist_gain(c: &[f64; 5]) -> f64 {
        (c[0] - c[1] + c[2]) / (1.0 - c[3] + c[4])
    }

    #[test]
    fn the_k_weighting_at_48_khz_equals_the_published_table() {
        let (pre, rlb) = KWeight::coefficients(48_000.0);
        for (derived, table) in pre.iter().zip(PRE.iter()).chain(rlb.iter().zip(RLB.iter())) {
            assert!((derived - table).abs() < 1e-9, "{derived} vs {table}");
        }
    }

    #[test]
    fn the_prototypes_are_a_4_db_shelf_near_1682_hz_and_a_38_hz_high_pass() {
        let pre = Prototype::from_digital(&PRE, TABLE_RATE);
        assert!((pre.f0_hz - 1681.97).abs() < 0.01, "{pre:?}");
        assert!((pre.q - 0.70718).abs() < 1e-4, "{pre:?}");
        assert!((pre.vh - 10f64.powf(4.0 / 20.0)).abs() < 1e-3, "{pre:?}");
        assert!((pre.vl - 1.0).abs() < 1e-8, "{pre:?}");
        let rlb = Prototype::from_digital(&RLB, TABLE_RATE);
        assert!((rlb.f0_hz - 38.135).abs() < 0.01, "{rlb:?}");
        assert!((rlb.q - 0.5003).abs() < 1e-3, "{rlb:?}");
        assert_eq!(rlb.vl, 0.0, "an exact zero at DC");
    }

    #[test]
    fn the_k_weighting_is_stable_and_finite_at_any_rate() {
        // A rate at or under twice a stage's corner must not reach the pole of `tan`.
        for rate in [
            1u32, 76, 3_364, 8_000, 22_050, 44_100, 48_000, 96_000, 192_000, 384_000,
        ] {
            let (pre, rlb) = KWeight::coefficients(f64::from(rate));
            for c in [pre, rlb] {
                assert!(c.iter().all(|v| v.is_finite()), "{rate} Hz: {c:?}");
                // Jury's conditions for a biquad's poles to lie inside the unit circle.
                assert!(c[4].abs() < 1.0, "{rate} Hz: {c:?}");
                assert!(c[3].abs() < 1.0 + c[4], "{rate} Hz: {c:?}");
            }
        }
    }

    #[test]
    fn the_analog_response_holds_at_every_rate() {
        // Below 4 kHz, where the bilinear warp is small, each rate matches the 48 kHz response.
        for rate in [44_100.0f64, 96_000.0, 192_000.0] {
            let (pre, rlb) = KWeight::coefficients(rate);
            for hz in [20.0f64, 38.0, 100.0, 997.0, 1682.0, 4000.0] {
                let at = |c: &[f64; 5], rate: f64| biquad_gain2(c, std::f64::consts::TAU * hz / rate);
                let here = at(&pre, rate) * at(&rlb, rate);
                let table = at(&PRE, TABLE_RATE) * at(&RLB, TABLE_RATE);
                let error_db = 10.0 * (here / table).log10();
                assert!(error_db.abs() < 0.05, "{rate} Hz at {hz} Hz: {error_db} dB");
            }
        }
    }

    #[test]
    fn the_annex_2_table_is_the_published_filter() {
        // A typo would show up in one of these: the coefficients are 13-bit fractions, the phases
        // mirror each other, and the 48-tap filter is a ±0.11 dB passband and a −39.8 dB stopband.
        for phase in &ANNEX_2_PHASES {
            for coeff in phase {
                assert_eq!((coeff * 8192.0).fract(), 0.0, "{coeff}");
            }
        }
        for tap in 0..TRUE_PEAK_TAPS {
            assert_eq!(
                ANNEX_2_PHASES[0][tap],
                ANNEX_2_PHASES[3][TRUE_PEAK_TAPS - 1 - tap]
            );
            assert_eq!(
                ANNEX_2_PHASES[1][tap],
                ANNEX_2_PHASES[2][TRUE_PEAK_TAPS - 1 - tap]
            );
        }
        let gain_db = |hz: f64| {
            let w = std::f64::consts::TAU * hz / 192_000.0;
            let (mut re, mut im) = (0.0f64, 0.0f64);
            for tap in 0..TRUE_PEAK_TAPS {
                for (phase, row) in ANNEX_2_PHASES.iter().enumerate() {
                    let at = w * (4 * tap + phase) as f64;
                    re += row[tap] * at.cos();
                    im -= row[tap] * at.sin();
                }
            }
            20.0 * ((re * re + im * im).sqrt() / 4.0).log10()
        };
        for hz in (0..20_000).step_by(50) {
            assert!(
                gain_db(f64::from(hz)).abs() < 0.12,
                "{hz} Hz: {}",
                gain_db(f64::from(hz))
            );
        }
        for hz in (28_000..96_000).step_by(50) {
            assert!(
                gain_db(f64::from(hz)) < -39.8,
                "{hz} Hz: {}",
                gain_db(f64::from(hz))
            );
        }
    }

    #[test]
    fn the_half_sample_phase_is_flat_where_a_96_khz_signal_has_energy() {
        let row = half_sample_phase();
        assert!((row.iter().sum::<f64>() - 1.0).abs() < 1e-12);
        for fraction in [0.05f64, 0.1, 0.2, 0.25, 0.3] {
            let w = std::f64::consts::TAU * fraction;
            let (mut re, mut im) = (0.0f64, 0.0f64);
            for (tap, coeff) in row.iter().enumerate() {
                re += coeff * (w * tap as f64).cos();
                im -= coeff * (w * tap as f64).sin();
            }
            let db = 20.0 * (re * re + im * im).sqrt().log10();
            assert!(db.abs() < 0.05, "{fraction} of the rate: {db} dB");
        }
    }

    #[test]
    fn the_k_weighting_is_a_plus_4_db_shelf_before_a_high_pass() {
        // Stage 1 (pre-filter): 0 dB at DC, ≈ +4 dB at Nyquist (a high shelf).
        assert!((dc_gain(&PRE) - 1.0).abs() < 1e-6);
        assert!((nyquist_gain(&PRE) - 10f64.powf(4.0 / 20.0)).abs() < 1e-3);
        // Stage 2 (RLB): a high-pass — an exact zero at DC, ≈ unity at Nyquist.
        assert!(dc_gain(&RLB).abs() < 1e-12);
        assert!((nyquist_gain(&RLB) - 1.0).abs() < 0.01);
    }

    #[test]
    fn the_meter_matches_the_analytical_k_weighting_response() {
        for hz in [997.0f32, 1000.0, 4000.0, 10_000.0] {
            let amp = 0.2f32;
            let samples = sine(hz, 0.5, amp);
            let mut meter = LoudnessMeter::new(1, 48_000).expect("valid");
            meter.push(&samples);
            let lufs = meter.momentary_lufs();
            // Measured power gain: mean square of the filtered sine over the raw mean square.
            let measured_gain2 =
                10f64.powf((f64::from(lufs) - LOUDNESS_OFFSET_DB) / 10.0) / f64::from(amp * amp / 2.0);
            let w = std::f64::consts::TAU * f64::from(hz) / 48_000.0;
            let analytical_gain2 = biquad_gain2(&PRE, w) * biquad_gain2(&RLB, w);
            assert!(
                (measured_gain2 - analytical_gain2).abs() < analytical_gain2 * 1e-3,
                "at {hz} Hz: measured {measured_gain2}, analytical {analytical_gain2}"
            );
        }
    }

    #[test]
    fn doubling_amplitude_raises_loudness_six_lu() {
        let quiet = measure_loudness(&sine(1000.0, 1.0, 0.1), 1, 48_000).expect("valid");
        let loud = measure_loudness(&sine(1000.0, 1.0, 0.2), 1, 48_000).expect("valid");
        assert!(
            (loud.integrated_lufs - quiet.integrated_lufs - 6.0206).abs() < 0.01,
            "{} vs {}",
            loud.integrated_lufs,
            quiet.integrated_lufs
        );
    }

    #[test]
    fn two_identical_channels_raise_loudness_three_lu() {
        let mono = sine(1000.0, 1.0, 0.2);
        let stereo: Vec<f32> = mono.iter().flat_map(|&s| [s, s]).collect();
        let mono_loud = measure_loudness(&mono, 1, 48_000).expect("valid");
        let stereo_loud = measure_loudness(&stereo, 2, 48_000).expect("valid");
        assert!(
            (stereo_loud.integrated_lufs - mono_loud.integrated_lufs - 3.0103).abs() < 0.01,
            "{} vs {}",
            stereo_loud.integrated_lufs,
            mono_loud.integrated_lufs
        );
    }

    #[test]
    fn silence_has_no_integrated_loudness() {
        // Digital silence never passes the −70 LUFS absolute gate, so there is no gated value.
        let silent = measure_loudness(&[0.0; 48_000], 1, 48_000).expect("valid");
        assert!(silent.integrated_lufs.is_infinite() && silent.integrated_lufs < 0.0);
    }

    #[test]
    fn a_loud_burst_amid_silence_reads_at_the_burst_level() {
        // One 400 ms burst (~ −14 LUFS) then 3 s of exact silence. An ungated mean would sit
        // around −23 LUFS (the silence drags it down); the absolute and relative gates throw the
        // silent and ring-down blocks away, so the reading stays near the burst.
        let mut burst = sine(1000.0, 0.4, 0.2);
        burst.extend(std::iter::repeat_n(0.0, 48_000 * 3));
        let loud = measure_loudness(&burst, 1, 48_000).expect("valid");
        assert!(loud.integrated_lufs > -20.0, "{}", loud.integrated_lufs);
    }

    #[test]
    fn the_loudness_range_leaves_out_passages_20_lu_below_the_mean() {
        // 6 s loud, then 6 s 40 dB quieter. The quiet readings pass the −70 LUFS absolute gate
        // but sit about 37 LU below the power mean, so EBU Tech 3342's relative gate leaves them
        // out: the range covers the loud part and the fade between (well under 20 LU), not the
        // full 40 LU step an absolute-gate-only range reports.
        let mut samples = sine(1000.0, 6.0, 0.5);
        samples.extend(sine(1000.0, 6.0, 0.005));
        let loud = measure_loudness(&samples, 1, 48_000).expect("valid");
        assert!(
            loud.loudness_range_lu > 1.0 && loud.loudness_range_lu < 20.0,
            "{}",
            loud.loudness_range_lu
        );
    }

    #[test]
    fn the_loudness_range_of_a_constant_signal_is_zero() {
        let samples = sine(1000.0, 6.0, 0.2);
        let loud = measure_loudness(&samples, 1, 48_000).expect("valid");
        assert!(loud.loudness_range_lu < 0.3, "{}", loud.loudness_range_lu);
    }

    #[test]
    fn normalize_gain_maps_a_measurement_to_its_target() {
        assert!((normalize_gain(-23.0, -16.0) - 10f32.powf(7.0 / 20.0)).abs() < 1e-6);
        assert!((normalize_gain(-16.0, -23.0) - 10f32.powf(-7.0 / 20.0)).abs() < 1e-6);
        assert_eq!(normalize_gain(f32::NAN, -23.0), 1.0);
        assert_eq!(normalize_gain(-200.0, -23.0), 10f32.powf(60.0 / 20.0));
    }

    #[test]
    fn the_true_peak_catches_an_inter_sample_peak() {
        // A 12 kHz sine at phase π/4 samples at ±0.7071 but peaks at 1.0 between samples.
        let samples: Vec<f32> = (0..4_800)
            .map(|i| (i as f32 * std::f32::consts::TAU / 4.0 + std::f32::consts::FRAC_PI_4).sin())
            .collect();
        let loud = measure_loudness(&samples, 1, 48_000).expect("valid");
        // Above the sample peak (−3.01 dB) and close to the true 1.0 (0 dB).
        assert!(loud.true_peak_db > -1.0, "{}", loud.true_peak_db);
        assert!(loud.true_peak_db < 0.5, "{}", loud.true_peak_db);
    }

    /// Audit v034 docs L2: the oversampler measures the stream's first and last sample pairs too.
    /// Two full-scale samples crest above 0 dBTP between them wherever they sit, so a transient at
    /// the very start or end reads the same true peak as the same transient mid-stream.
    #[test]
    fn the_true_peak_counts_the_first_and_last_samples() {
        let pad = vec![0.0f32; 4_800];
        let pulse = [1.0f32, 1.0];
        let middle: Vec<f32> = pad.iter().chain(&pulse).chain(&pad).copied().collect();
        let start: Vec<f32> = pulse.iter().chain(&pad).copied().collect();
        let end: Vec<f32> = pad.iter().chain(&pulse).copied().collect();
        let tp = |s: &[f32]| measure_loudness(s, 1, 48_000).expect("valid").true_peak_db;
        let reference = tp(&middle);
        assert!(
            reference > 0.5,
            "the pair crests between its samples: {reference}"
        );
        assert!(
            (tp(&start) - reference).abs() < 1e-3,
            "start {} vs {reference}",
            tp(&start)
        );
        assert!(
            (tp(&end) - reference).abs() < 1e-3,
            "end {} vs {reference}",
            tp(&end)
        );
    }

    #[test]
    fn bad_inputs_are_rejected_with_errors() {
        assert_eq!(measure_loudness(&[], 1, 48_000), Err(LoudnessError::Empty));
        assert_eq!(measure_loudness(&[0.0], 1, 0), Err(LoudnessError::ZeroSampleRate));
        assert_eq!(
            measure_loudness(&[0.0; 3], 3, 48_000),
            Err(LoudnessError::UnsupportedChannels(3))
        );
    }

    /// A meter sizes its 400 ms and 3 s windows from the rate before it sees a sample, so a rate
    /// past [`crate::MAX_SOUND_SAMPLE_RATE`] is refused there too (a direct caller, not only the
    /// decode path); the maximum itself builds.
    #[test]
    fn a_meter_refuses_a_rate_past_the_maximum() {
        let max = crate::MAX_SOUND_SAMPLE_RATE;
        assert_eq!(
            LoudnessMeter::new(2, max + 1).err(),
            Some(LoudnessError::SampleRateTooHigh(max + 1))
        );
        assert_eq!(
            measure_loudness(&[0.0; 4], 2, 1_000_000).err(),
            Some(LoudnessError::SampleRateTooHigh(1_000_000))
        );
        assert!(LoudnessMeter::new(2, max).is_ok(), "the maximum builds");
    }
}
