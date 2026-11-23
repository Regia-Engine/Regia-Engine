// SPDX-License-Identifier: MPL-2.0
//! Convolution reverb: a measured or designed impulse response (IR) applied on a bus
//! (`BusEffect::Convolution`, `docs/specs/16-audio.md` § 1) or by a reverb zone (§ 3d), ADR-2721.
//!
//! # Algorithm
//!
//! Uniformly partitioned overlap-save convolution in the frequency domain (a frequency-domain
//! delay line): the IR is cut into partitions of [`CONVOLUTION_PARTITION`] frames, each
//! zero-padded to `2 × partition` and transformed once when the IR is prepared. Every block of
//! `partition` input frames is transformed together with the block before it, stored in a ring
//! of past input spectra, and the output block is the inverse transform of
//! `Σₚ X(k − p) · Hₚ`, of which the last `partition` samples are kept (overlap-save). Sources:
//! W. G. Gardner, "Efficient Convolution without Input-Output Delay", JAES 43(3), 1995 (the
//! partitioned scheme and its cost analysis), and F. Wefers, "Partitioned convolution algorithms
//! for real-time auralization", PhD thesis, RWTH Aachen, 2015, ch. 5 (uniformly partitioned
//! overlap-save, UPOLS). The code is Regia Engine's own.
//!
//! Both input channels go through one complex transform: left as the real part, right as the
//! imaginary part. With a mono IR the products stay separable (`h` is real). A stereo IR is
//! applied as `L·Hₗ + i R·Hᵣ = Z·(Hₗ + Hᵣ)/2 + Z̃·(Hₗ − Hᵣ)/2`, where `Z̃(k) = conj Z(N − k)`, so
//! the per-block cost stays one forward and one inverse transform (the identity follows from the
//! Hermitian symmetry of real signals' spectra; original derivation).
//!
//! # Latency
//!
//! The effect buffers one partition before it can transform it, so the wet path is
//! [`CONVOLUTION_PARTITION`] frames late (5.3 ms at 48 kHz). The predelay absorbs it: a
//! predelay of at least one partition is exact, a shorter one becomes one partition. The dry
//! path is not delayed.
//!
//! # Real-time rules
//!
//! Everything the audio thread touches is built on the game thread ([`ConvolutionSlot::new`],
//! span `convolution_prepare`) and handed over through a bounded channel; the audio thread never
//! allocates, frees or locks. A retired slot goes back through a second channel and is freed on
//! the game thread ([`ConvolutionHandle::collect`]).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use crossbeam_channel::{Receiver, Sender, TrySendError};
use kira::{
    Frame,
    effect::{Effect, EffectBuilder},
    info::Info,
};
use rustfft::{Fft, FftPlanner, num_complex::Complex};

/// Frames per partition (and per processed block). The FFT size is twice this.
pub const CONVOLUTION_PARTITION: usize = 256;

/// The FFT size of one partition.
const FFT_LEN: usize = CONVOLUTION_PARTITION * 2;

/// Longest predelay accepted, seconds.
pub const MAX_PREDELAY_SECS: f32 = 0.5;

/// Default crossfade between two IRs, seconds (zone changes, `audio.bus_effect_set`).
pub const DEFAULT_IR_CROSSFADE_SECS: f32 = 0.25;

/// Longest crossfade accepted, seconds.
pub const MAX_IR_CROSSFADE_SECS: f32 = 10.0;

/// Commands that may wait for the audio thread before the handle refuses more.
const COMMAND_CAPACITY: usize = 4;

/// How much convolution a device may run: the IR length cap and whether zones may use IRs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ConvolutionTier {
    /// No convolution: bus convolvers pass only the dry signal and zones use their preset reverb.
    Low,
    /// Phones and handhelds: IRs up to [`ConvolutionTier::max_ir_secs`] = 1.5 s.
    Mobile,
    /// Desktop: IRs up to 4 s.
    #[default]
    Desktop,
}

impl ConvolutionTier {
    /// The platform default: `Mobile` on Android, `Desktop` elsewhere.
    #[must_use]
    pub fn platform_default() -> Self {
        if cfg!(target_os = "android") {
            Self::Mobile
        } else {
            Self::Desktop
        }
    }

    /// The hard IR length cap, seconds (0 on `Low`). At 48 kHz the desktop cap is 750
    /// partitions: about 0.4 M complex multiply-adds per 256-frame block for a mono IR.
    #[must_use]
    pub fn max_ir_secs(self) -> f32 {
        match self {
            Self::Low => 0.0,
            Self::Mobile => 1.5,
            Self::Desktop => 4.0,
        }
    }

    /// The IR length cap in frames at `sample_rate`.
    #[must_use]
    pub fn max_ir_frames(self, sample_rate: u32) -> usize {
        (f64::from(self.max_ir_secs()) * f64::from(sample_rate)).floor() as usize
    }

    /// The tier's name (`low`, `mobile`, `desktop`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Mobile => "mobile",
            Self::Desktop => "desktop",
        }
    }

    /// The tier named `name` (inverse of [`Self::name`]).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "low" => Self::Low,
            "mobile" => Self::Mobile,
            "desktop" => Self::Desktop,
            _ => return None,
        })
    }
}

/// Why an impulse response was refused. Every variant has a stable lowercase
/// [`ConvolutionError::code`] (`book/agents/audio-convolution.md`).
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum ConvolutionError {
    /// The IR has no samples (or the file no bytes).
    #[error("the impulse response is empty")]
    Empty,
    /// The IR file ends before the length its header declares, or cannot be decoded.
    #[error("the impulse response file is truncated or unreadable: {0}")]
    Truncated(String),
    /// Every sample is zero: convolving with it would only silence the wet path.
    #[error("the impulse response is silent")]
    Silent,
    /// A NaN or infinite sample.
    #[error("the impulse response has non-finite samples")]
    NonFinite,
    /// Not one or two channels, or channels of different lengths.
    #[error(
        "the impulse response has {0} channels of unequal or unsupported shape; use 1 or 2 equal channels"
    )]
    Channels(usize),
    /// A sample rate outside 8 kHz to 192 kHz.
    #[error("the impulse response sample rate {0} Hz is outside 8000..=192000")]
    SampleRate(u32),
    /// Longer than the tier allows at the output rate.
    #[error("the impulse response has {frames} frames; the {tier} tier allows {cap}")]
    TooLong {
        /// Frames at the output rate.
        frames: usize,
        /// The tier cap in frames.
        cap: usize,
        /// The tier's name.
        tier: &'static str,
    },
    /// The tier runs no convolution.
    #[error("convolution is off on the low tier")]
    TierDisabled,
    /// The effect's command queue is full (the audio thread has not caught up).
    #[error("the convolver is busy; try again after the current crossfade")]
    Busy,
}

impl ConvolutionError {
    /// The stable error code (`ir_empty`, `ir_truncated`, `ir_silent`, `ir_non_finite`,
    /// `ir_channels`, `ir_sample_rate`, `ir_too_long`, `ir_tier_disabled`, `convolver_busy`).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Empty => "ir_empty",
            Self::Truncated(_) => "ir_truncated",
            Self::Silent => "ir_silent",
            Self::NonFinite => "ir_non_finite",
            Self::Channels(_) => "ir_channels",
            Self::SampleRate(_) => "ir_sample_rate",
            Self::TooLong { .. } => "ir_too_long",
            Self::TierDisabled => "ir_tier_disabled",
            Self::Busy => "convolver_busy",
        }
    }
}

/// A validated impulse response: one or two equal-length channels at a sample rate.
#[derive(Clone, Debug, PartialEq)]
pub struct ImpulseResponse {
    channels: Vec<Arc<[f32]>>,
    sample_rate: u32,
}

impl ImpulseResponse {
    /// A mono IR (applied to both channels).
    ///
    /// ```
    /// # use regia_audio::ImpulseResponse;
    /// let ir = ImpulseResponse::mono(vec![1.0, 0.5, 0.25], 48_000).expect("valid");
    /// assert_eq!(ir.frames(), 3);
    /// assert!(ImpulseResponse::mono(vec![], 48_000).is_err());
    /// ```
    pub fn mono(samples: Vec<f32>, sample_rate: u32) -> Result<Self, ConvolutionError> {
        Self::from_channels(vec![samples], sample_rate)
    }

    /// A stereo IR: the left channel convolves the left input, the right the right input.
    pub fn stereo(left: Vec<f32>, right: Vec<f32>, sample_rate: u32) -> Result<Self, ConvolutionError> {
        Self::from_channels(vec![left, right], sample_rate)
    }

    /// One or two equal-length channels.
    pub fn from_channels(channels: Vec<Vec<f32>>, sample_rate: u32) -> Result<Self, ConvolutionError> {
        if !(crate::MIXDOWN_SAMPLE_RATES).contains(&sample_rate) {
            return Err(ConvolutionError::SampleRate(sample_rate));
        }
        let count = channels.len();
        if count == 0 || count > 2 {
            return Err(ConvolutionError::Channels(count));
        }
        let frames = channels[0].len();
        if channels.iter().any(|c| c.len() != frames) {
            return Err(ConvolutionError::Channels(count));
        }
        if frames == 0 {
            return Err(ConvolutionError::Empty);
        }
        if channels.iter().flatten().any(|s| !s.is_finite()) {
            return Err(ConvolutionError::NonFinite);
        }
        if channels.iter().flatten().all(|s| *s == 0.0) {
            return Err(ConvolutionError::Silent);
        }
        Ok(Self {
            channels: channels.into_iter().map(Arc::from).collect(),
            sample_rate,
        })
    }

    /// The IR of a decoded sound. A stereo sound whose channels are identical becomes mono.
    /// A streaming sound is decoded into memory first.
    pub fn from_sound(sound: &crate::SoundAsset) -> Result<Self, ConvolutionError> {
        let decoded = sound
            .decode_to_memory()
            .map_err(|e| ConvolutionError::Truncated(e.to_string()))?;
        let (frames, rate) = decoded
            .static_frames()
            .ok_or_else(|| ConvolutionError::Truncated("the sound has no decoded frames".into()))?;
        Self::from_frames(frames, rate)
    }

    /// The IR in an encoded sound file (WAV, Ogg Vorbis, FLAC: whatever the cooked sound uses).
    /// A WAV whose `data` chunk is shorter than its header declares is refused as
    /// [`ConvolutionError::Truncated`] (a decoder would otherwise play what is there).
    pub fn from_encoded_bytes(bytes: &[u8]) -> Result<Self, ConvolutionError> {
        if bytes.is_empty() {
            return Err(ConvolutionError::Empty);
        }
        check_wav_complete(bytes)?;
        let data =
            kira::sound::static_sound::StaticSoundData::from_cursor(std::io::Cursor::new(bytes.to_vec()))
                .map_err(|e| ConvolutionError::Truncated(e.to_string()))?;
        Self::from_frames(&data.frames, data.sample_rate)
    }

    fn from_frames(frames: &[Frame], sample_rate: u32) -> Result<Self, ConvolutionError> {
        let left: Vec<f32> = frames.iter().map(|f| f.left).collect();
        let right: Vec<f32> = frames.iter().map(|f| f.right).collect();
        if left == right {
            Self::mono(left, sample_rate)
        } else {
            Self::stereo(left, right, sample_rate)
        }
    }

    /// Frames per channel.
    #[must_use]
    pub fn frames(&self) -> usize {
        self.channels[0].len()
    }

    /// 1 (mono) or 2 (stereo).
    #[must_use]
    pub fn channel_count(&self) -> usize {
        self.channels.len()
    }

    /// The samples of channel `index`.
    #[must_use]
    pub fn channel(&self, index: usize) -> Option<&[f32]> {
        self.channels.get(index).map(|c| &c[..])
    }

    /// The IR's sample rate.
    #[must_use]
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Length, seconds.
    #[must_use]
    pub fn duration_secs(&self) -> f64 {
        self.frames() as f64 / f64::from(self.sample_rate)
    }

    /// The same IR at `sample_rate` (band-limited sinc interpolation, [`resample_sinc`]); itself
    /// when the rates match.
    ///
    /// The taps are scaled by `from / to`: [`resample_sinc`] keeps the amplitude of a signal, but
    /// an IR's gain is the sum of its taps, and the tap count scales with the rate. Without the
    /// scale a 48 kHz IR heard at 96 kHz is 6 dB louder (ADR-7602; the HRIR load does the same).
    #[must_use]
    pub fn resampled(&self, sample_rate: u32) -> Self {
        if sample_rate == self.sample_rate || sample_rate == 0 {
            return self.clone();
        }
        let scale = (f64::from(self.sample_rate) / f64::from(sample_rate)) as f32;
        Self {
            channels: self
                .channels
                .iter()
                .map(|c| {
                    let mut taps = resample_sinc(c, self.sample_rate, sample_rate);
                    taps.iter_mut().for_each(|tap| *tap *= scale);
                    Arc::from(taps)
                })
                .collect(),
            sample_rate,
        }
    }
}

/// Refuses a RIFF/WAVE file whose `data` chunk declares more bytes than the file holds. Other
/// containers pass (their decoder reports damage itself).
fn check_wav_complete(bytes: &[u8]) -> Result<(), ConvolutionError> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Ok(());
    }
    let mut at = 12usize;
    while let Some(header) = bytes.get(at..at + 8) {
        let id = &header[0..4];
        let size = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
        let body = at + 8;
        if id == b"data" {
            if body.saturating_add(size) > bytes.len() {
                return Err(ConvolutionError::Truncated(format!(
                    "the data chunk declares {size} bytes, {} are present",
                    bytes.len().saturating_sub(body)
                )));
            }
            if size == 0 {
                return Err(ConvolutionError::Empty);
            }
            return Ok(());
        }
        // Chunks are word aligned.
        at = body.saturating_add(size).saturating_add(size & 1);
    }
    Err(ConvolutionError::Truncated(
        "the WAVE file has no data chunk".into(),
    ))
}

/// Taps on each side of the resampling kernel.
const SINC_HALF_TAPS: f64 = 32.0;

/// Band-limited resampling of a short signal by windowed-sinc interpolation (J. O. Smith,
/// "Digital Audio Resampling Home Page", CCRMA, the bandlimited interpolation formula; Blackman
/// window). Downsampling lowers the cutoff to the new Nyquist rate. The output is
/// `ceil(len · to / from)` samples with no added delay. For load time, not the audio thread.
#[must_use]
pub fn resample_sinc(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || input.is_empty() || from == 0 || to == 0 {
        return input.to_vec();
    }
    let step = f64::from(from) / f64::from(to);
    // Cutoff relative to the input rate (1 = input Nyquist).
    let cutoff = (1.0 / step).min(1.0);
    let half = SINC_HALF_TAPS / cutoff;
    let out_len = ((input.len() as f64) / step).ceil() as usize;
    (0..out_len)
        .map(|n| {
            let x = n as f64 * step;
            let first = (x - half).ceil().max(0.0) as usize;
            let last = ((x + half).floor() as usize).min(input.len() - 1);
            let mut acc = 0.0f64;
            for (k, sample) in input.iter().enumerate().take(last + 1).skip(first) {
                let t = x - k as f64;
                let arg = std::f64::consts::PI * t * cutoff;
                let sinc = if arg.abs() < 1e-12 { 1.0 } else { arg.sin() / arg };
                let w = t / half;
                let window = 0.42
                    + 0.5 * (std::f64::consts::PI * w).cos()
                    + 0.08 * (2.0 * std::f64::consts::PI * w).cos();
                acc += f64::from(*sample) * sinc * window * cutoff;
            }
            acc as f32
        })
        .collect()
}

/// An IR transformed for the partitioned convolver at one sample rate; shared (`Arc`) by every
/// convolver that uses it.
pub struct ConvolutionKernel {
    /// Per partition, `(Hₗ + Hᵣ)/2` (the IR's spectrum for a mono IR), partitions back to back.
    sum: Vec<Complex<f32>>,
    /// Per partition, `(Hₗ − Hᵣ)/2`; `None` for a mono IR.
    diff: Option<Vec<Complex<f32>>>,
    partitions: usize,
    frames: usize,
    channels: usize,
    sample_rate: u32,
    fft: Arc<dyn Fft<f32>>,
    ifft: Arc<dyn Fft<f32>>,
}

impl std::fmt::Debug for ConvolutionKernel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConvolutionKernel")
            .field("partitions", &self.partitions)
            .field("frames", &self.frames)
            .field("channels", &self.channels)
            .field("sample_rate", &self.sample_rate)
            .finish_non_exhaustive()
    }
}

impl ConvolutionKernel {
    /// Prepares `ir` for convolution at `sample_rate` under `tier`: resamples it, checks the
    /// tier cap and transforms every partition. Runs on the game thread (span
    /// `convolution_prepare`).
    pub fn prepare(
        ir: &ImpulseResponse,
        sample_rate: u32,
        tier: ConvolutionTier,
    ) -> Result<Arc<Self>, ConvolutionError> {
        let _span = tracing::info_span!("convolution_prepare", frames = ir.frames(), sample_rate).entered();
        if tier == ConvolutionTier::Low {
            return Err(ConvolutionError::TierDisabled);
        }
        if !(crate::MIXDOWN_SAMPLE_RATES).contains(&sample_rate) {
            return Err(ConvolutionError::SampleRate(sample_rate));
        }
        let ir = ir.resampled(sample_rate);
        let cap = tier.max_ir_frames(sample_rate);
        if ir.frames() > cap {
            return Err(ConvolutionError::TooLong {
                frames: ir.frames(),
                cap,
                tier: tier.name(),
            });
        }
        let mut planner = FftPlanner::new();
        let fft = planner.plan_fft_forward(FFT_LEN);
        let ifft = planner.plan_fft_inverse(FFT_LEN);
        let partitions = ir.frames().div_ceil(CONVOLUTION_PARTITION);
        let spectra = |samples: &[f32]| -> Vec<Complex<f32>> {
            let mut out = vec![Complex::new(0.0, 0.0); partitions * FFT_LEN];
            for (p, part) in out.chunks_mut(FFT_LEN).enumerate() {
                let start = p * CONVOLUTION_PARTITION;
                let end = (start + CONVOLUTION_PARTITION).min(samples.len());
                for (slot, s) in part.iter_mut().zip(&samples[start..end]) {
                    *slot = Complex::new(*s, 0.0);
                }
                fft.process(part);
            }
            out
        };
        let (sum, diff) = if ir.channel_count() == 1 {
            (spectra(&ir.channels[0]), None)
        } else {
            let left = spectra(&ir.channels[0]);
            let right = spectra(&ir.channels[1]);
            let sum = left.iter().zip(&right).map(|(l, r)| (l + r) * 0.5).collect();
            let diff = left.iter().zip(&right).map(|(l, r)| (l - r) * 0.5).collect();
            (sum, Some(diff))
        };
        Ok(Arc::new(Self {
            sum,
            diff,
            partitions,
            frames: ir.frames(),
            channels: ir.channel_count(),
            sample_rate,
            fft,
            ifft,
        }))
    }

    /// Partitions of [`CONVOLUTION_PARTITION`] frames.
    #[must_use]
    pub fn partitions(&self) -> usize {
        self.partitions
    }

    /// IR frames at the kernel's rate.
    #[must_use]
    pub fn frames(&self) -> usize {
        self.frames
    }

    /// IR channels (1 or 2).
    #[must_use]
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// The rate the kernel was prepared for.
    #[must_use]
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
}

/// One partitioned convolver: the kernel plus its own input history. Allocation-free after
/// [`Convolver::new`].
pub struct Convolver {
    kernel: Arc<ConvolutionKernel>,
    /// Ring of past input spectra, `partitions × FFT_LEN`.
    fdl: Vec<Complex<f32>>,
    /// The ring slot holding the newest spectrum.
    newest: usize,
    /// The previous block's input (left, right).
    history: Vec<Complex<f32>>,
    work: Vec<Complex<f32>>,
    acc: Vec<Complex<f32>>,
    scratch: Vec<Complex<f32>>,
}

impl Convolver {
    /// A convolver over `kernel` with silent history.
    #[must_use]
    pub fn new(kernel: Arc<ConvolutionKernel>) -> Self {
        let scratch = kernel
            .fft
            .get_inplace_scratch_len()
            .max(kernel.ifft.get_inplace_scratch_len());
        let zero = Complex::new(0.0, 0.0);
        Self {
            fdl: vec![zero; kernel.partitions.max(1) * FFT_LEN],
            newest: 0,
            history: vec![zero; CONVOLUTION_PARTITION],
            work: vec![zero; FFT_LEN],
            acc: vec![zero; FFT_LEN],
            scratch: vec![zero; scratch],
            kernel,
        }
    }

    /// The kernel.
    #[must_use]
    pub fn kernel(&self) -> &Arc<ConvolutionKernel> {
        &self.kernel
    }

    /// Convolves one block of [`CONVOLUTION_PARTITION`] stereo frames. `input` and `output` must
    /// be exactly one partition long; other lengths leave `output` untouched.
    pub fn process_block(&mut self, input: &[Frame], output: &mut [Frame]) {
        if input.len() != CONVOLUTION_PARTITION || output.len() != CONVOLUTION_PARTITION {
            return;
        }
        let parts = self.kernel.partitions.max(1);
        // [previous block | this block], left in the real part and right in the imaginary.
        let (older, newer) = self.work.split_at_mut(CONVOLUTION_PARTITION);
        older.copy_from_slice(&self.history);
        for ((slot, hist), frame) in newer.iter_mut().zip(self.history.iter_mut()).zip(input) {
            *slot = Complex::new(frame.left, frame.right);
            *hist = *slot;
        }
        self.kernel
            .fft
            .process_with_scratch(&mut self.work, &mut self.scratch);
        self.newest = (self.newest + parts - 1) % parts;
        let at = self.newest * FFT_LEN;
        self.fdl[at..at + FFT_LEN].copy_from_slice(&self.work);

        self.acc.fill(Complex::new(0.0, 0.0));
        for p in 0..parts {
            let slot = (self.newest + p) % parts;
            let x = &self.fdl[slot * FFT_LEN..(slot + 1) * FFT_LEN];
            let h = &self.kernel.sum[p * FFT_LEN..(p + 1) * FFT_LEN];
            for ((acc, x), h) in self.acc.iter_mut().zip(x).zip(h) {
                *acc += x * h;
            }
            if let Some(diff) = &self.kernel.diff {
                let d = &diff[p * FFT_LEN..(p + 1) * FFT_LEN];
                for (k, (acc, d)) in self.acc.iter_mut().zip(d).enumerate() {
                    let mirrored = x[(FFT_LEN - k) % FFT_LEN].conj();
                    *acc += mirrored * d;
                }
            }
        }
        self.kernel
            .ifft
            .process_with_scratch(&mut self.acc, &mut self.scratch);
        let norm = 1.0 / FFT_LEN as f32;
        for (out, y) in output.iter_mut().zip(&self.acc[CONVOLUTION_PARTITION..]) {
            *out = Frame {
                left: y.re * norm,
                right: y.im * norm,
            };
        }
    }
}

/// A convolver with its predelay line: what the effect runs and crossfades between. Built on the
/// game thread, moved to the audio thread whole.
pub struct ConvolutionSlot {
    convolver: Convolver,
    /// Predelay ring (frames); length is the delay plus one block.
    delay: Vec<Frame>,
    delay_frames: usize,
    delay_pos: usize,
    delayed: Vec<Frame>,
    wet: Vec<Frame>,
    /// The predelay asked for, frames (the partition latency is part of it).
    predelay_frames: usize,
}

impl ConvolutionSlot {
    /// A slot over `kernel` with `predelay_secs` of predelay (clamped to
    /// 0..=[`MAX_PREDELAY_SECS`]; NaN is 0). The wet path is `max(predelay, partition)` late.
    #[must_use]
    pub fn new(kernel: Arc<ConvolutionKernel>, predelay_secs: f32) -> Self {
        let predelay = if predelay_secs.is_finite() {
            predelay_secs.clamp(0.0, MAX_PREDELAY_SECS)
        } else {
            0.0
        };
        let predelay_frames = (f64::from(predelay) * f64::from(kernel.sample_rate)).round() as usize;
        let delay_frames = predelay_frames.saturating_sub(CONVOLUTION_PARTITION);
        Self {
            convolver: Convolver::new(kernel),
            delay: vec![Frame::ZERO; delay_frames + 1],
            delay_frames,
            delay_pos: 0,
            delayed: vec![Frame::ZERO; CONVOLUTION_PARTITION],
            wet: vec![Frame::ZERO; CONVOLUTION_PARTITION],
            predelay_frames,
        }
    }

    /// The kernel.
    #[must_use]
    pub fn kernel(&self) -> &Arc<ConvolutionKernel> {
        self.convolver.kernel()
    }

    /// The wet path's delay, frames: `max(predelay, partition)`.
    #[must_use]
    pub fn wet_delay_frames(&self) -> usize {
        self.predelay_frames.max(CONVOLUTION_PARTITION)
    }

    fn process(&mut self, input: &[Frame]) {
        if self.delay_frames == 0 {
            self.delayed.copy_from_slice(input);
        } else {
            let len = self.delay.len();
            for (out, frame) in self.delayed.iter_mut().zip(input) {
                self.delay[self.delay_pos] = *frame;
                let read = (self.delay_pos + len - self.delay_frames) % len;
                *out = self.delay[read];
                self.delay_pos = (self.delay_pos + 1) % len;
            }
        }
        self.convolver.process_block(&self.delayed, &mut self.wet);
    }
}

/// Counters a convolution effect publishes (`audio.convolution_stats`).
#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Serialize)]
pub struct ConvolutionStats {
    /// Blocks of [`CONVOLUTION_PARTITION`] frames convolved.
    pub blocks: u64,
    /// IR swaps the audio thread took (each starts a crossfade).
    pub swaps: u64,
    /// Crossfades finished.
    pub crossfades_done: u64,
    /// A crossfade is running.
    pub crossfading: bool,
    /// IR frames of the active slot (0 = none: only the dry signal passes).
    pub ir_frames: usize,
    /// IR channels of the active slot.
    pub ir_channels: usize,
    /// Partitions of the active slot.
    pub partitions: usize,
    /// The wet path's delay, frames.
    pub wet_delay_frames: usize,
    /// The partition latency, frames ([`CONVOLUTION_PARTITION`]).
    pub latency_frames: usize,
    /// The mixer runs at another rate than the active kernel was prepared for: the wet path is
    /// silent until a kernel at [`ConvolutionStats::mixer_rate`] arrives (the audio system
    /// re-prepares it on the next frame).
    pub rate_mismatch: bool,
    /// The rate the mixer runs the effect at, Hz.
    pub mixer_rate: u32,
    /// The wet/dry mix target (0 = dry, 1 = wet).
    pub mix: f32,
    /// The largest change between consecutive output samples seen, a click detector.
    pub max_step: f32,
}

/// State the game thread and the audio thread share (atomics only).
#[derive(Debug, Default)]
struct Shared {
    blocks: AtomicU64,
    swaps: AtomicU64,
    crossfades_done: AtomicU64,
    crossfading: AtomicBool,
    ir_frames: AtomicU64,
    ir_channels: AtomicU32,
    partitions: AtomicU64,
    wet_delay_frames: AtomicU64,
    rate_mismatch: AtomicBool,
    /// The rate the mixer runs this effect at (0 until kira initialises it).
    mixer_rate: AtomicU32,
    mix: AtomicU32,
    max_step: AtomicU32,
}

/// A change for the audio thread.
enum Command {
    /// Crossfade to `slot` (`None`: to silence) over `fade_frames`.
    Swap {
        slot: Option<Box<ConvolutionSlot>>,
        fade_frames: usize,
    },
}

/// The game thread's side of a convolution effect: swap IRs, set the mix, read the stats.
pub struct ConvolutionHandle {
    commands: Sender<Command>,
    retired: Receiver<Box<ConvolutionSlot>>,
    shared: Arc<Shared>,
    sample_rate: u32,
    ir_name: Option<String>,
    predelay_secs: f32,
    /// The rate of the kernel last sent (0: none).
    kernel_rate: u32,
}

impl std::fmt::Debug for ConvolutionHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConvolutionHandle")
            .field("ir_name", &self.ir_name)
            .field("sample_rate", &self.sample_rate)
            .finish_non_exhaustive()
    }
}

impl ConvolutionHandle {
    /// Crossfades to `kernel` (named `name` for reports) with `predelay_secs` over `fade_secs`;
    /// `None` fades the wet path out. The slot is built here, on the calling thread.
    pub fn set_kernel(
        &mut self,
        name: Option<&str>,
        kernel: Option<Arc<ConvolutionKernel>>,
        predelay_secs: f32,
        fade_secs: f32,
    ) -> Result<(), ConvolutionError> {
        self.collect();
        let fade = if fade_secs.is_finite() {
            fade_secs.clamp(0.0, MAX_IR_CROSSFADE_SECS)
        } else {
            DEFAULT_IR_CROSSFADE_SECS
        };
        let fade_frames = (f64::from(fade) * f64::from(self.mixer_rate())).round() as usize;
        let rate = kernel.as_ref().map_or(0, |k| k.sample_rate);
        let slot = kernel.map(|k| Box::new(ConvolutionSlot::new(k, predelay_secs)));
        match self.commands.try_send(Command::Swap { slot, fade_frames }) {
            Ok(()) => {
                self.ir_name = name.map(str::to_string);
                self.predelay_secs = predelay_secs;
                self.kernel_rate = rate;
                Ok(())
            }
            // The returned command (and its slot) is dropped here, on the game thread.
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => Err(ConvolutionError::Busy),
        }
    }

    /// Sets the wet/dry mix target (0 = dry only, 1 = wet only; NaN is ignored). The effect ramps
    /// to it over one block.
    pub fn set_mix(&self, mix: f32) {
        if mix.is_finite() {
            self.shared
                .mix
                .store(mix.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
        }
    }

    /// Frees slots the audio thread retired. Called by every other method; call it once a frame
    /// when nothing else touches the handle.
    pub fn collect(&self) -> usize {
        self.retired.try_iter().count()
    }

    /// The rate the mixer runs the effect at (the build rate until kira reports its own).
    #[must_use]
    pub fn mixer_rate(&self) -> u32 {
        match self.shared.mixer_rate.load(Ordering::Relaxed) {
            0 => self.sample_rate,
            rate => rate,
        }
    }

    /// Records the predelay and IR name an effect was built with when it got no kernel (an IR
    /// registered later is sent with them).
    pub(crate) fn note_predelay(&mut self, predelay_secs: f32) {
        self.predelay_secs = predelay_secs;
    }

    /// The rate of the kernel last sent (0: none sent).
    #[must_use]
    pub fn kernel_rate(&self) -> u32 {
        self.kernel_rate
    }

    /// The name of the IR last sent (`None`: none).
    #[must_use]
    pub fn ir_name(&self) -> Option<&str> {
        self.ir_name.as_deref()
    }

    /// The predelay last sent, seconds.
    #[must_use]
    pub fn predelay_secs(&self) -> f32 {
        self.predelay_secs
    }

    /// The counters the audio thread publishes.
    #[must_use]
    pub fn stats(&self) -> ConvolutionStats {
        let s = &self.shared;
        ConvolutionStats {
            blocks: s.blocks.load(Ordering::Relaxed),
            swaps: s.swaps.load(Ordering::Relaxed),
            crossfades_done: s.crossfades_done.load(Ordering::Relaxed),
            crossfading: s.crossfading.load(Ordering::Relaxed),
            ir_frames: s.ir_frames.load(Ordering::Relaxed) as usize,
            ir_channels: s.ir_channels.load(Ordering::Relaxed) as usize,
            partitions: s.partitions.load(Ordering::Relaxed) as usize,
            wet_delay_frames: s.wet_delay_frames.load(Ordering::Relaxed) as usize,
            latency_frames: CONVOLUTION_PARTITION,
            rate_mismatch: s.rate_mismatch.load(Ordering::Relaxed),
            mixer_rate: s.mixer_rate.load(Ordering::Relaxed),
            mix: f32::from_bits(s.mix.load(Ordering::Relaxed)),
            max_step: f32::from_bits(s.max_step.load(Ordering::Relaxed)),
        }
    }
}

/// Builds a convolution effect for a kira track.
pub struct ConvolutionEffectBuilder {
    effect: ConvolutionEffect,
}

impl ConvolutionEffectBuilder {
    /// An effect at `sample_rate` (the mixer's rate) starting with `slot` (or dry only) at `mix`.
    /// Returns the builder and the game thread's handle.
    #[must_use]
    pub fn new(
        sample_rate: u32,
        slot: Option<ConvolutionSlot>,
        mix: f32,
        ir_name: Option<&str>,
    ) -> (Self, ConvolutionHandle) {
        let (commands, inbox) = crossbeam_channel::bounded(COMMAND_CAPACITY);
        // Room for every slot that can be in flight: the queue, the active pair and one retired.
        let (outbox, retired) = crossbeam_channel::bounded(COMMAND_CAPACITY + 3);
        let mix = if mix.is_finite() { mix.clamp(0.0, 1.0) } else { 0.0 };
        let shared = Arc::new(Shared::default());
        shared.mix.store(mix.to_bits(), Ordering::Relaxed);
        shared.mixer_rate.store(sample_rate, Ordering::Relaxed);
        let kernel_rate = slot.as_ref().map_or(0, |s| s.kernel().sample_rate);
        let predelay_secs = slot
            .as_ref()
            .map_or(0.0, |s| s.predelay_frames as f32 / sample_rate.max(1) as f32);
        let effect = ConvolutionEffect {
            current: slot.map(Box::new),
            incoming: None,
            fade_pos: 0,
            fade_len: 0,
            fading: false,
            retired: None,
            inbox,
            outbox,
            shared: Arc::clone(&shared),
            mixer_rate: sample_rate,
            input: vec![Frame::ZERO; CONVOLUTION_PARTITION],
            pending: vec![Frame::ZERO; CONVOLUTION_PARTITION],
            fill: 0,
            mix_from: mix,
            mix_to: mix,
            wet_tail: 0,
            last_out: Frame::ZERO,
            max_step: 0.0,
        };
        effect.publish_slot();
        (
            Self { effect },
            ConvolutionHandle {
                commands,
                retired,
                shared,
                sample_rate,
                ir_name: ir_name.map(str::to_string),
                predelay_secs,
                kernel_rate,
            },
        )
    }

    /// The effect without kira's track (offline tests drive [`Effect::process`] directly).
    #[must_use]
    pub fn into_effect(self) -> Box<dyn Effect> {
        Box::new(self.effect)
    }
}

impl EffectBuilder for ConvolutionEffectBuilder {
    type Handle = ();

    fn build(self) -> (Box<dyn Effect>, Self::Handle) {
        (Box::new(self.effect), ())
    }
}

/// The kira effect. See the module docs.
struct ConvolutionEffect {
    current: Option<Box<ConvolutionSlot>>,
    incoming: Option<Box<ConvolutionSlot>>,
    fade_pos: usize,
    fade_len: usize,
    fading: bool,
    /// A slot waiting for room in the outbox.
    retired: Option<Box<ConvolutionSlot>>,
    inbox: Receiver<Command>,
    outbox: Sender<Box<ConvolutionSlot>>,
    shared: Arc<Shared>,
    /// The rate the mixer runs at; a slot prepared for another rate stays silent.
    mixer_rate: u32,
    /// The block being filled.
    input: Vec<Frame>,
    /// The previous block's wet output, drained while the next fills.
    pending: Vec<Frame>,
    fill: usize,
    mix_from: f32,
    mix_to: f32,
    /// Blocks of wet output still to drain after the last slot went away.
    wet_tail: usize,
    last_out: Frame,
    max_step: f32,
}

impl ConvolutionEffect {
    fn publish_slot(&self) {
        let slot = self.incoming.as_ref().or(self.current.as_ref());
        let s = &self.shared;
        let (frames, channels, parts, delay) = slot.map_or((0, 0, 0, 0), |slot| {
            let k = slot.kernel();
            (k.frames, k.channels, k.partitions, slot.wet_delay_frames())
        });
        s.ir_frames.store(frames as u64, Ordering::Relaxed);
        s.ir_channels.store(channels as u32, Ordering::Relaxed);
        s.partitions.store(parts as u64, Ordering::Relaxed);
        s.wet_delay_frames.store(delay as u64, Ordering::Relaxed);
        s.crossfading.store(self.fading, Ordering::Relaxed);
    }

    /// Hands a finished slot back to the game thread (never frees it here).
    fn retire(&mut self, slot: Box<ConvolutionSlot>) {
        match self.outbox.try_send(slot) {
            Ok(()) => {}
            Err(TrySendError::Full(slot) | TrySendError::Disconnected(slot)) => {
                // Keep it; no new command is taken until it is gone.
                self.retired = Some(slot);
            }
        }
    }

    /// Takes one waiting command when nothing is fading and nothing waits to be retired.
    fn take_command(&mut self) {
        if let Some(slot) = self.retired.take() {
            self.retire(slot);
        }
        if self.fading || self.retired.is_some() {
            return;
        }
        let Ok(Command::Swap { slot, fade_frames }) = self.inbox.try_recv() else {
            return;
        };
        self.shared.swaps.fetch_add(1, Ordering::Relaxed);
        if self.current.is_none() && slot.is_none() {
            // Nothing to fade between: drop the empty command (no allocation is involved).
            return;
        }
        // The fade is whole blocks long, at least one block.
        self.fade_len = fade_frames.div_ceil(CONVOLUTION_PARTITION).max(1) * CONVOLUTION_PARTITION;
        self.fade_pos = 0;
        self.fading = true;
        // An empty incoming slot fades the current one out.
        self.incoming = slot;
        if self.incoming.is_none() {
            self.wet_tail = 0;
        }
        self.publish_slot();
    }

    /// Convolves the filled block into `pending`.
    fn run_block(&mut self) {
        self.take_command();
        let rate = self.mixer_rate;
        let runs = |slot: &ConvolutionSlot| slot.kernel().sample_rate == rate;
        let mut any = false;
        let mut mismatch = false;
        for slot in [self.current.as_mut(), self.incoming.as_mut()]
            .into_iter()
            .flatten()
        {
            if runs(slot) {
                slot.process(&self.input);
                any = true;
            } else {
                // Silent until a kernel at the mixer's rate arrives.
                slot.wet.fill(Frame::ZERO);
                mismatch = true;
            }
        }
        self.shared.rate_mismatch.store(mismatch, Ordering::Relaxed);
        if !any {
            // Wet path silent: drain one block of zeros after the last slot went away.
            self.pending.fill(Frame::ZERO);
            self.wet_tail = self.wet_tail.saturating_sub(1);
        } else if self.fading {
            let half_pi = std::f32::consts::FRAC_PI_2;
            for (i, out) in self.pending.iter_mut().enumerate() {
                let t = ((self.fade_pos + i + 1) as f32 / self.fade_len as f32).min(1.0);
                // Equal power: the two reverbs are uncorrelated.
                let (g_out, g_in) = ((t * half_pi).cos(), (t * half_pi).sin());
                let old = self.current.as_ref().map_or(Frame::ZERO, |s| s.wet[i]);
                let new = self.incoming.as_ref().map_or(Frame::ZERO, |s| s.wet[i]);
                *out = old * g_out + new * g_in;
            }
            self.fade_pos += CONVOLUTION_PARTITION;
            if self.fade_pos >= self.fade_len {
                self.fading = false;
                let finished = std::mem::replace(&mut self.current, self.incoming.take());
                if let Some(old) = finished {
                    self.retire(old);
                }
                self.shared.crossfades_done.fetch_add(1, Ordering::Relaxed);
                if self.current.is_none() {
                    self.wet_tail = 1;
                }
                self.publish_slot();
            }
        } else if let Some(slot) = self.current.as_ref() {
            self.pending.copy_from_slice(&slot.wet);
        }
        self.shared.blocks.fetch_add(1, Ordering::Relaxed);
        self.mix_from = self.mix_to;
        self.mix_to = f32::from_bits(self.shared.mix.load(Ordering::Relaxed));
        self.shared
            .max_step
            .store(self.max_step.to_bits(), Ordering::Relaxed);
    }

    /// True when the effect would leave the signal exactly as it is: fully dry, no slot, no fade,
    /// no wet tail left and nothing queued. An unused bus convolver costs nothing and changes no
    /// sample. (A wet-only convolver with no IR is not idle: it outputs silence.)
    fn idle(&self) -> bool {
        self.mix_from == 0.0
            && self.mix_to == 0.0
            && f32::from_bits(self.shared.mix.load(Ordering::Relaxed)) == 0.0
            && self.current.is_none()
            && self.incoming.is_none()
            && !self.fading
            && self.wet_tail == 0
            && self.retired.is_none()
            && self.inbox.is_empty()
    }
}

impl Effect for ConvolutionEffect {
    fn init(&mut self, sample_rate: u32, _internal_buffer_size: usize) {
        self.on_change_sample_rate(sample_rate);
    }

    fn on_change_sample_rate(&mut self, sample_rate: u32) {
        self.mixer_rate = sample_rate;
        self.shared.mixer_rate.store(sample_rate, Ordering::Relaxed);
    }

    fn process(&mut self, input: &mut [Frame], _dt: f64, _info: &Info) {
        if self.idle() {
            return;
        }
        for frame in input.iter_mut() {
            let dry = *frame;
            self.input[self.fill] = dry;
            let wet = self.pending[self.fill];
            let t = (self.fill + 1) as f32 / CONVOLUTION_PARTITION as f32;
            let mix = self.mix_from + (self.mix_to - self.mix_from) * t;
            let out = dry * (1.0 - mix) + wet * mix;
            let step = (out.left - self.last_out.left)
                .abs()
                .max((out.right - self.last_out.right).abs());
            if step.is_finite() && step > self.max_step {
                self.max_step = step;
            }
            self.last_out = out;
            *frame = out;
            self.fill += 1;
            if self.fill == CONVOLUTION_PARTITION {
                self.fill = 0;
                self.run_block();
            }
        }
    }
}

/// Direct (time-domain) convolution of one channel, for tests and offline checks: `ir * input`,
/// `input.len()` samples long.
#[must_use]
pub fn direct_convolution(input: &[f32], ir: &[f32]) -> Vec<f32> {
    (0..input.len())
        .map(|n| {
            let taps = ir.len().min(n + 1);
            (0..taps)
                .map(|k| f64::from(ir[k]) * f64::from(input[n - k]))
                .sum::<f64>() as f32
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noise(len: usize, seed: u32) -> Vec<f32> {
        let mut state = seed;
        (0..len)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (state >> 8) as f32 / (1u32 << 24) as f32 - 0.5
            })
            .collect()
    }

    /// A decaying noise IR, `frames` long.
    fn decaying_ir(frames: usize, seed: u32) -> Vec<f32> {
        noise(frames, seed)
            .into_iter()
            .enumerate()
            .map(|(i, s)| s * (-(i as f32) / (frames as f32 / 6.0)).exp())
            .collect()
    }

    /// Runs `left`/`right` through a fresh convolver block by block.
    fn convolve(ir: &ImpulseResponse, left: &[f32], right: &[f32]) -> (Vec<f32>, Vec<f32>) {
        let kernel =
            ConvolutionKernel::prepare(ir, ir.sample_rate(), ConvolutionTier::Desktop).expect("kernel");
        let mut convolver = Convolver::new(kernel);
        let (mut out_l, mut out_r) = (Vec::new(), Vec::new());
        let mut out = vec![Frame::ZERO; CONVOLUTION_PARTITION];
        for (l, r) in left
            .chunks(CONVOLUTION_PARTITION)
            .zip(right.chunks(CONVOLUTION_PARTITION))
        {
            let mut block = vec![Frame::ZERO; CONVOLUTION_PARTITION];
            for ((frame, l), r) in block.iter_mut().zip(l).zip(r) {
                *frame = Frame { left: *l, right: *r };
            }
            convolver.process_block(&block, &mut out);
            out_l.extend(out.iter().map(|f| f.left));
            out_r.extend(out.iter().map(|f| f.right));
        }
        (out_l, out_r)
    }

    /// Error energy relative to the reference energy, dB.
    fn error_db(got: &[f32], want: &[f32]) -> f64 {
        let err: f64 = got
            .iter()
            .zip(want)
            .map(|(a, b)| (f64::from(*a) - f64::from(*b)).powi(2))
            .sum();
        let reference: f64 = want.iter().map(|b| f64::from(*b).powi(2)).sum();
        10.0 * (err / reference.max(1e-30)).log10()
    }

    #[test]
    fn a_unit_impulse_returns_the_ir() {
        let taps = decaying_ir(3_000, 11);
        let ir = ImpulseResponse::mono(taps.clone(), 48_000).expect("ir");
        let len = 4 * 1024;
        let mut impulse = vec![0.0f32; len];
        impulse[0] = 1.0;
        let (l, r) = convolve(&ir, &impulse, &impulse);
        let want: Vec<f32> = (0..len).map(|i| taps.get(i).copied().unwrap_or(0.0)).collect();
        assert!(error_db(&l, &want) < -80.0, "left {} dB", error_db(&l, &want));
        assert!(error_db(&r, &want) < -80.0, "right {} dB", error_db(&r, &want));
        // Every tail sample, not only the energy, sits below -80 dB of the IR's peak.
        let peak = taps.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        let worst = l
            .iter()
            .zip(&want)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(
            20.0 * (worst / peak).log10() < -80.0,
            "worst sample error {worst}"
        );
    }

    #[test]
    fn any_signal_matches_direct_convolution_mono_and_stereo() {
        let len = 6 * CONVOLUTION_PARTITION;
        let (in_l, in_r) = (noise(len, 3), noise(len, 5));
        // Mono IR, a length that is not a partition multiple.
        let taps = decaying_ir(700, 7);
        let ir = ImpulseResponse::mono(taps.clone(), 48_000).expect("ir");
        let (l, r) = convolve(&ir, &in_l, &in_r);
        assert!(error_db(&l, &direct_convolution(&in_l, &taps)) < -80.0);
        assert!(error_db(&r, &direct_convolution(&in_r, &taps)) < -80.0);
        // Stereo IR: left convolves left, right convolves right.
        let (tl, tr) = (decaying_ir(900, 13), decaying_ir(900, 17));
        let ir = ImpulseResponse::stereo(tl.clone(), tr.clone(), 48_000).expect("ir");
        let (l, r) = convolve(&ir, &in_l, &in_r);
        let el = error_db(&l, &direct_convolution(&in_l, &tl));
        let er = error_db(&r, &direct_convolution(&in_r, &tr));
        assert!(el < -80.0 && er < -80.0, "stereo errors {el} / {er} dB");
    }

    #[test]
    fn bad_irs_are_refused_with_codes() {
        assert_eq!(
            ImpulseResponse::mono(vec![], 48_000).map(|_| ()),
            Err(ConvolutionError::Empty)
        );
        assert_eq!(
            ImpulseResponse::mono(vec![0.0; 64], 48_000).map(|_| ()),
            Err(ConvolutionError::Silent)
        );
        assert_eq!(
            ImpulseResponse::mono(vec![1.0, f32::NAN], 48_000).map(|_| ()),
            Err(ConvolutionError::NonFinite)
        );
        assert_eq!(
            ImpulseResponse::stereo(vec![1.0], vec![1.0, 0.0], 48_000).map(|_| ()),
            Err(ConvolutionError::Channels(2))
        );
        assert_eq!(
            ImpulseResponse::mono(vec![1.0], 0).map(|_| ()),
            Err(ConvolutionError::SampleRate(0))
        );
        assert_eq!(
            ImpulseResponse::from_encoded_bytes(&[]).map(|_| ()),
            Err(ConvolutionError::Empty)
        );
        let codes = [
            ConvolutionError::Empty.code(),
            ConvolutionError::Truncated(String::new()).code(),
            ConvolutionError::TierDisabled.code(),
        ];
        assert_eq!(codes, ["ir_empty", "ir_truncated", "ir_tier_disabled"]);
    }

    #[test]
    fn tier_caps_hold() {
        let ir = ImpulseResponse::mono(decaying_ir(48_000 * 2, 1), 48_000).expect("2 s ir");
        assert!(ConvolutionKernel::prepare(&ir, 48_000, ConvolutionTier::Desktop).is_ok());
        assert!(matches!(
            ConvolutionKernel::prepare(&ir, 48_000, ConvolutionTier::Mobile),
            Err(ConvolutionError::TooLong { cap: 72_000, .. })
        ));
        assert_eq!(
            ConvolutionKernel::prepare(&ir, 48_000, ConvolutionTier::Low).map(|_| ()),
            Err(ConvolutionError::TierDisabled)
        );
        assert_eq!(ConvolutionTier::Desktop.max_ir_frames(48_000), 192_000);
        if cfg!(target_os = "android") {
            assert_eq!(ConvolutionTier::platform_default(), ConvolutionTier::Mobile);
        }
    }

    #[test]
    fn resampling_keeps_the_arrival_and_the_dc_gain() {
        // An impulse at 44.1 kHz frame 441 (10 ms) lands at 48 kHz frame 480.
        let mut taps = vec![0.0f32; 4_410];
        taps[441] = 1.0;
        let ir = ImpulseResponse::mono(taps, 44_100).expect("ir");
        let up = ir.resampled(48_000);
        let ch = up.channel(0).expect("channel");
        assert_eq!(up.frames(), 4_800);
        let peak = ch
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
            .map(|(i, _)| i);
        assert_eq!(peak, Some(480));
        // A constant stays constant (DC gain 1) away from the ends.
        let flat = resample_sinc(&vec![0.5; 2_000], 48_000, 44_100);
        assert!(flat[200..1_600].iter().all(|s| (s - 0.5).abs() < 1e-3));
    }

    /// |H(f)| of an IR's taps at `freq` Hz: the gain a tone at `freq` gets through the reverb.
    fn response_at(taps: &[f32], sample_rate: u32, freq: f64) -> f64 {
        let w = std::f64::consts::TAU * freq / f64::from(sample_rate);
        let (re, im) = taps
            .iter()
            .enumerate()
            .fold((0.0f64, 0.0f64), |(re, im), (n, h)| {
                let phase = w * n as f64;
                (re + f64::from(*h) * phase.cos(), im - f64::from(*h) * phase.sin())
            });
        re.hypot(im)
    }

    /// The reverb's gain must not move with the mix rate an IR is resampled to: a 48 kHz IR
    /// heard at 44.1, 48 or 96 kHz gives tones the same level (|H(f)| at 250 Hz and 1 kHz, the
    /// tap sum for a low-passed IR). The impulse energy (sum of squares) is printed too: it
    /// scales with about `from / to` once the gain is kept, because the same band is spread over
    /// more or fewer taps. The IR has a 2 ms onset, as a recorded one does: an IR that starts at
    /// full level on its first tap loses half the sinc kernel there, which alone moves |H(1 kHz)|
    /// by about 0.1 dB at 96 kHz (kernel truncation at the edge, not the gain this test checks).
    #[test]
    fn a_resampled_ir_keeps_its_gain_at_every_mix_rate() {
        // A decaying, deterministic noise tail, 0.25 s at 48 kHz with a 2 ms (96-tap) onset,
        // low-passed by a 3-tap average so its spectrum sits under 20 kHz (the band every rate
        // keeps).
        let mut state = 0x2545_f491u32;
        let raw: Vec<f32> = (0..12_000)
            .map(|n| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let noise = (state >> 8) as f32 / (1u32 << 24) as f32 - 0.5;
                let onset = (n as f32 / 96.0).min(1.0);
                noise * onset * (-(n as f32) / 2_400.0).exp()
            })
            .collect();
        let taps: Vec<f32> = (0..raw.len())
            .map(|n| {
                let a = raw[n.saturating_sub(1)];
                let c = raw.get(n + 1).copied().unwrap_or(0.0);
                (a + raw[n] + c) / 3.0
            })
            .collect();
        let ir = ImpulseResponse::mono(taps, 48_000).expect("ir");
        let source = ir.channel(0).expect("channel").to_vec();
        for rate in [44_100u32, 48_000, 96_000] {
            let at = ir.resampled(rate);
            let ch = at.channel(0).expect("channel");
            let energy: f64 = ch.iter().map(|s| f64::from(*s).powi(2)).sum();
            let source_energy: f64 = source.iter().map(|s| f64::from(*s).powi(2)).sum();
            println!(
                "{rate} Hz: energy x{:.4}, |H(250)| x{:.4}, |H(1k)| x{:.4}",
                energy / source_energy,
                response_at(ch, rate, 250.0) / response_at(&source, 48_000, 250.0),
                response_at(ch, rate, 1_000.0) / response_at(&source, 48_000, 1_000.0),
            );
            for freq in [250.0, 1_000.0] {
                let db = 20.0 * (response_at(ch, rate, freq) / response_at(&source, 48_000, freq)).log10();
                assert!(
                    db.abs() < 0.1,
                    "a 48 kHz IR resampled to {rate} Hz changes a {freq} Hz tone by {db:+.2} dB"
                );
            }
        }
    }

    #[test]
    fn a_truncated_wav_is_refused() {
        // 16-bit mono WAV header declaring 100 data bytes, with 10 present.
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36u32 + 100).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&48_000u32.to_le_bytes());
        wav.extend_from_slice(&96_000u32.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&100u32.to_le_bytes());
        wav.extend_from_slice(&[0x10; 10]);
        let err = ImpulseResponse::from_encoded_bytes(&wav).expect_err("truncated");
        assert_eq!(err.code(), "ir_truncated", "{err}");
        // Cut inside the header: no data chunk.
        let err = ImpulseResponse::from_encoded_bytes(&wav[..30]).expect_err("cut header");
        assert_eq!(err.code(), "ir_truncated");
    }
}
