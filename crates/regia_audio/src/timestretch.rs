// SPDX-License-Identifier: MPL-2.0
//! Pitch-preserving time stretch by WSOLA (waveform-similarity overlap-add), ADR-2721.
//!
//! Playback rate in kira changes pitch with speed. WSOLA changes the speed alone: the output is
//! built from Hann-windowed segments of [`WSOLA_WINDOW`] frames overlap-added every
//! [`WSOLA_HOP`] frames (50 % overlap, where a periodic Hann window sums to exactly one). The
//! input advances `rate × hop` per output hop, and each segment is taken not at its nominal
//! place but within ±[`WSOLA_TOLERANCE`] frames of it, where it best continues the previous
//! segment (the highest normalized cross-correlation with the input that followed the previous
//! segment), so the waveform stays continuous and the pitch is kept. Source: W. Verhelst and M.
//! Roelands, "An overlap-add technique based on waveform similarity (WSOLA) for high quality
//! time-scale modification of speech", Proc. IEEE ICASSP 1993, pp. 554–557. The code is Regia
//! Engine's own; the start-up (a virtual segment one hop before the first frame, so the first
//! output hop is the input itself) and the coarse-then-fine search are original.
//!
//! Two front ends share the core: [`time_stretch`] for offline use (the cook, tools, the agent
//! probe), and [`TimeStretchSoundData`], a kira sound that plays a decoded sound at a rate that
//! may change while it plays (`Audio::play_time_stretched`). Rate 1 is a pass-through in both:
//! the offline call returns its input unchanged and a voice that has never left rate 1 reads its
//! frames directly.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use kira::{
    Frame,
    info::Info,
    sound::{Sound, SoundData},
};

/// Frames per analysis/synthesis window.
pub const WSOLA_WINDOW: usize = 1024;

/// Frames per synthesis hop (half the window).
pub const WSOLA_HOP: usize = WSOLA_WINDOW / 2;

/// How far a segment may move from its nominal place, frames (8 ms at 48 kHz: more than half
/// the period of anything above 62 Hz).
pub const WSOLA_TOLERANCE: usize = 384;

/// Slowest rate accepted (twice as long).
pub const MIN_STRETCH_RATE: f32 = 0.5;

/// Fastest rate accepted (half as long).
pub const MAX_STRETCH_RATE: f32 = 2.0;

/// Why a stretch was refused; [`TimeStretchError::code`] is stable.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum TimeStretchError {
    /// The rate is not finite or outside [`MIN_STRETCH_RATE`]..=[`MAX_STRETCH_RATE`].
    #[error("time-stretch rate {0} is outside 0.5..=2")]
    Rate(f32),
    /// No samples.
    #[error("nothing to stretch")]
    Empty,
    /// Not 1 or 2 channels, or an interleaved buffer that is not whole frames.
    #[error("time stretch takes 1 or 2 interleaved channels in whole frames, got {0} channels")]
    Channels(usize),
}

impl TimeStretchError {
    /// `stretch_rate`, `stretch_empty` or `stretch_channels`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Rate(_) => "stretch_rate",
            Self::Empty => "stretch_empty",
            Self::Channels(_) => "stretch_channels",
        }
    }
}

/// `rate` if it is usable, else the error.
pub fn check_rate(rate: f32) -> Result<f32, TimeStretchError> {
    if rate.is_finite() && (MIN_STRETCH_RATE..=MAX_STRETCH_RATE).contains(&rate) {
        Ok(rate)
    } else {
        Err(TimeStretchError::Rate(rate))
    }
}

/// Where the core reads its input.
trait Source {
    /// The frame at `index` (silence outside the source unless it loops).
    fn frame(&self, index: i64) -> Frame;
}

struct SliceSource<'a> {
    frames: &'a [Frame],
    looping: bool,
}

impl Source for SliceSource<'_> {
    #[inline]
    fn frame(&self, index: i64) -> Frame {
        let len = self.frames.len() as i64;
        if len == 0 {
            return Frame::ZERO;
        }
        if self.looping {
            self.frames[index.rem_euclid(len) as usize]
        } else if (0..len).contains(&index) {
            self.frames[index as usize]
        } else {
            Frame::ZERO
        }
    }
}

/// The WSOLA state: the overlap accumulator and the input positions. Allocation-free after
/// [`Wsola::new`].
struct Wsola {
    window: Vec<f32>,
    /// Overlap accumulator, one window long.
    acc: Vec<Frame>,
    /// Input start of the previous segment.
    prev: i64,
    /// Nominal input position of the next segment.
    pos: f64,
    /// Search step of the coarse pass (1: exhaustive).
    coarse_step: usize,
}

impl Wsola {
    /// A core whose first output hop is the input from `start` on (see the module docs).
    fn new(start: f64, coarse_step: usize, source: &dyn Source) -> Self {
        let window: Vec<f32> = (0..WSOLA_WINDOW)
            .map(|n| 0.5 - 0.5 * (std::f32::consts::TAU * n as f32 / WSOLA_WINDOW as f32).cos())
            .collect();
        let mut core = Self {
            window,
            acc: vec![Frame::ZERO; WSOLA_WINDOW],
            prev: 0,
            pos: 0.0,
            coarse_step: coarse_step.max(1),
        };
        core.restart(start, source);
        core
    }

    /// Restarts in place (no allocation) so the next output hop is the input from `start` on.
    fn restart(&mut self, start: f64, source: &dyn Source) {
        self.prev = start.round() as i64 - WSOLA_HOP as i64;
        self.pos = start;
        // The virtual segment one hop before the start: its falling half meets the first real
        // segment's rising half.
        self.acc.fill(Frame::ZERO);
        for (n, slot) in self.acc.iter_mut().enumerate().skip(WSOLA_HOP) {
            *slot = source.frame(self.prev + n as i64) * self.window[n];
        }
        self.acc.rotate_left(WSOLA_HOP);
        self.acc[WSOLA_HOP..].fill(Frame::ZERO);
    }

    /// Normalized cross-correlation of the overlap halves at `candidate` and `target` (mono).
    fn score(source: &dyn Source, candidate: i64, target: i64) -> f32 {
        let (mut xy, mut yy) = (0.0f32, 0.0f32);
        for n in 0..WSOLA_HOP as i64 {
            let x = source.frame(target + n);
            let y = source.frame(candidate + n);
            let (xm, ym) = (x.left + x.right, y.left + y.right);
            xy += xm * ym;
            yy += ym * ym;
        }
        if yy <= 1e-12 { 0.0 } else { xy / yy.sqrt() }
    }

    /// The segment start near `nominal` that best continues the previous segment.
    fn search(&self, source: &dyn Source, nominal: i64) -> i64 {
        let target = self.prev + WSOLA_HOP as i64;
        let tol = WSOLA_TOLERANCE as i64;
        let mut best = (nominal, f32::NEG_INFINITY);
        let mut delta = -tol;
        while delta <= tol {
            let s = Self::score(source, nominal + delta, target);
            if s > best.1 {
                best = (nominal + delta, s);
            }
            delta += self.coarse_step as i64;
        }
        if self.coarse_step > 1 {
            let centre = best.0;
            let reach = self.coarse_step as i64 - 1;
            for candidate in centre - reach..=centre + reach {
                if candidate == centre || (candidate - nominal).abs() > tol {
                    continue;
                }
                let s = Self::score(source, candidate, target);
                if s > best.1 {
                    best = (candidate, s);
                }
            }
        }
        best.0
    }

    /// Produces the next [`WSOLA_HOP`] output frames into `out` and advances the input by
    /// `rate × hop`.
    fn next_hop(&mut self, source: &dyn Source, rate: f32, out: &mut [Frame]) {
        let nominal = self.pos.round() as i64;
        let start = self.search(source, nominal);
        for (n, slot) in self.acc.iter_mut().enumerate() {
            *slot += source.frame(start + n as i64) * self.window[n];
        }
        out[..WSOLA_HOP].copy_from_slice(&self.acc[..WSOLA_HOP]);
        self.acc.rotate_left(WSOLA_HOP);
        self.acc[WSOLA_HOP..].fill(Frame::ZERO);
        self.prev = start;
        self.pos += f64::from(rate) * WSOLA_HOP as f64;
    }
}

/// Stretches interleaved `samples` (`channels` = 1 or 2) by `rate` without changing the pitch:
/// the output is `round(frames / rate)` frames. Rate 1 returns the input unchanged. For the cook
/// and tools (allocates); playing voices use [`TimeStretchSoundData`].
///
/// ```
/// # use regia_audio::time_stretch;
/// let tone: Vec<f32> = (0..48_000).map(|i| (i as f32 * 0.0576).sin()).collect();
/// let slow = time_stretch(&tone, 1, 0.5).expect("stretch");
/// assert_eq!(slow.len(), 96_000);
/// assert_eq!(time_stretch(&tone, 1, 1.0).expect("same"), tone);
/// ```
pub fn time_stretch(samples: &[f32], channels: usize, rate: f32) -> Result<Vec<f32>, TimeStretchError> {
    let rate = check_rate(rate)?;
    if channels == 0 || channels > 2 || !samples.len().is_multiple_of(channels) {
        return Err(TimeStretchError::Channels(channels));
    }
    if samples.is_empty() {
        return Err(TimeStretchError::Empty);
    }
    if rate == 1.0 {
        return Ok(samples.to_vec());
    }
    let _span = tracing::info_span!("time_stretch", frames = samples.len() / channels, rate).entered();
    let frames: Vec<Frame> = if channels == 1 {
        samples.iter().map(|s| Frame { left: *s, right: *s }).collect()
    } else {
        let (pairs, _) = samples.as_chunks::<2>();
        pairs
            .iter()
            .map(|[left, right]| Frame {
                left: *left,
                right: *right,
            })
            .collect()
    };
    let out = stretch_frames(&frames, rate);
    Ok(if channels == 1 {
        out.iter().map(|f| f.left).collect()
    } else {
        out.iter().flat_map(|f| [f.left, f.right]).collect()
    })
}

/// The offline WSOLA pass shared by [`time_stretch`] and [`stretch_asset`]: `frames` at `rate`
/// (already checked, not 1), `round(len / rate)` frames out.
fn stretch_frames(frames: &[Frame], rate: f32) -> Vec<Frame> {
    let source = SliceSource {
        frames,
        looping: false,
    };
    let out_frames = (frames.len() as f64 / f64::from(rate)).round() as usize;
    let mut core = Wsola::new(0.0, 2, &source);
    let mut out = Vec::with_capacity(out_frames + WSOLA_HOP);
    let mut hop = vec![Frame::ZERO; WSOLA_HOP];
    while out.len() < out_frames {
        core.next_hop(&source, rate, &mut hop);
        out.extend_from_slice(&hop);
    }
    out.truncate(out_frames);
    out
}

/// Most source frames one [`stretch_asset`] call reads: ten minutes at 48 kHz. A longer range is
/// refused (`stretch_too_long`) rather than holding gigabytes of stretched audio.
pub const MAX_STRETCH_ASSET_FRAMES: usize = 48_000 * 600;

/// The section hook (ADR-3197): renders `source_secs` of `asset` from `start_secs` (seconds into
/// the sound) at a pitch-preserving `rate` into a new in-memory sound, with the asset's loudness
/// gain. The result is `round(frames / rate)` frames at the asset's sample rate, so playing it
/// plainly from `t` seconds is the stretched range at `t`: a seek into it gives exactly the
/// samples playing up to that point gave, and a render-out reads it like any decoded sound.
/// The range is clipped to the sound. Rate 1 returns the range unchanged (bit for bit).
///
/// The sequencer renders pitch-preserving sound sections ahead with it into a bounded cache
/// (`regia_framework::sequencer`). It allocates and costs about one WSOLA pass, so call it off
/// the frame's critical path for long ranges.
///
/// # Errors
/// [`AudioError::Invalid`](crate::AudioError::Invalid) starting with `stretch_rate` (outside 0.5..=2 or not finite),
/// `stretch_empty` (nothing of the sound in the range) or `stretch_too_long` (more than
/// [`MAX_STRETCH_ASSET_FRAMES`] source frames); a decode error for a stream that cannot be
/// decoded into memory.
pub fn stretch_asset(
    asset: &crate::SoundAsset,
    start_secs: f64,
    source_secs: f64,
    rate: f32,
) -> Result<crate::SoundAsset, crate::AudioError> {
    let invalid = |e: TimeStretchError| crate::AudioError::Invalid(format!("{}: {e}", e.code()));
    let rate = check_rate(rate).map_err(invalid)?;
    let decoded = asset.decode_to_memory()?;
    let (frames, sample_rate) = decoded
        .static_frames()
        .ok_or_else(|| crate::AudioError::load(asset.label(), "no decoded frames"))?;
    let at = |secs: f64| -> usize {
        if secs.is_finite() {
            (secs.max(0.0) * f64::from(sample_rate))
                .round()
                .min(frames.len() as f64) as usize
        } else {
            0
        }
    };
    let from = at(start_secs);
    let to = if source_secs.is_finite() {
        at(start_secs.max(0.0) + source_secs.max(0.0))
    } else {
        frames.len()
    };
    if to <= from {
        return Err(invalid(TimeStretchError::Empty));
    }
    if to - from > MAX_STRETCH_ASSET_FRAMES {
        return Err(crate::AudioError::Invalid(format!(
            "stretch_too_long: {} source frames; the limit is {MAX_STRETCH_ASSET_FRAMES}",
            to - from
        )));
    }
    let range = &frames[from..to];
    let _span = tracing::info_span!("stretch_asset", frames = range.len(), rate).entered();
    let out: std::sync::Arc<[Frame]> = if rate == 1.0 {
        std::sync::Arc::from(range)
    } else {
        stretch_frames(range, rate).into()
    };
    let data = kira::sound::static_sound::StaticSoundData {
        sample_rate,
        frames: out,
        slice: None,
        settings: kira::sound::static_sound::StaticSoundSettings::new(),
    };
    Ok(crate::SoundAsset::from_static_data(data).with_loudness_gain_db(asset.loudness_gain_db()))
}

/// Shared control of a time-stretched voice.
#[derive(Debug)]
struct StretchShared {
    rate: AtomicU32,
    stop: AtomicBool,
    finished: AtomicBool,
    /// Input position, frames.
    position: AtomicU64,
    /// WSOLA hops produced.
    hops: AtomicU64,
}

/// The game thread's handle of a time-stretched voice.
#[derive(Clone, Debug)]
pub struct TimeStretchHandle {
    shared: Arc<StretchShared>,
    sample_rate: u32,
}

impl TimeStretchHandle {
    /// Changes the rate (0.5..=2); the next hop uses it.
    pub fn set_rate(&self, rate: f32) -> Result<(), TimeStretchError> {
        let rate = check_rate(rate)?;
        self.shared.rate.store(rate.to_bits(), Ordering::Relaxed);
        Ok(())
    }

    /// The rate.
    #[must_use]
    pub fn rate(&self) -> f32 {
        f32::from_bits(self.shared.rate.load(Ordering::Relaxed))
    }

    /// Stops the voice with a [`crate::DECLICK_FADE`] fade.
    pub fn stop(&self) {
        self.shared.stop.store(true, Ordering::Relaxed);
    }

    /// True once the voice has ended (stopped, or past the end of a sound that does not loop).
    #[must_use]
    pub fn finished(&self) -> bool {
        self.shared.finished.load(Ordering::Relaxed)
    }

    /// The input position, seconds.
    #[must_use]
    pub fn position_secs(&self) -> f64 {
        self.shared.position.load(Ordering::Relaxed) as f64 / f64::from(self.sample_rate.max(1))
    }

    /// WSOLA hops produced (0 while the voice has stayed at rate 1).
    #[must_use]
    pub fn hops(&self) -> u64 {
        self.shared.hops.load(Ordering::Relaxed)
    }
}

/// A kira sound that plays decoded frames at a changeable, pitch-preserving rate.
pub struct TimeStretchSoundData {
    frames: Arc<[Frame]>,
    sample_rate: u32,
    rate: f32,
    looping: bool,
    gain: f32,
}

impl TimeStretchSoundData {
    /// `frames` (at the mixer's rate) played at `rate`, looping or once, at linear `gain`.
    pub fn new(
        frames: Arc<[Frame]>,
        sample_rate: u32,
        rate: f32,
        looping: bool,
        gain: f32,
    ) -> Result<Self, TimeStretchError> {
        let rate = check_rate(rate)?;
        if frames.is_empty() {
            return Err(TimeStretchError::Empty);
        }
        Ok(Self {
            frames,
            sample_rate,
            rate,
            looping,
            gain: if gain.is_finite() { gain.max(0.0) } else { 0.0 },
        })
    }
}

impl SoundData for TimeStretchSoundData {
    type Error = std::convert::Infallible;
    type Handle = TimeStretchHandle;

    fn into_sound(self) -> Result<(Box<dyn Sound>, Self::Handle), Self::Error> {
        let shared = Arc::new(StretchShared {
            rate: AtomicU32::new(self.rate.to_bits()),
            stop: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            position: AtomicU64::new(0),
            hops: AtomicU64::new(0),
        });
        let fade_frames = (crate::DECLICK_FADE.as_secs_f64() * f64::from(self.sample_rate)).ceil() as usize;
        // Built here, on the thread that starts the voice: the audio thread never allocates.
        let core = Wsola::new(
            0.0,
            2,
            &SliceSource {
                frames: &self.frames,
                looping: self.looping,
            },
        );
        let sound = TimeStretchSound {
            frames: self.frames,
            looping: self.looping,
            gain: self.gain,
            core,
            stretching: false,
            hop: vec![Frame::ZERO; WSOLA_HOP],
            hop_read: WSOLA_HOP,
            direct: 0,
            fade_left: fade_frames.max(1),
            fade_len: fade_frames.max(1),
            shared: Arc::clone(&shared),
            done: false,
        };
        Ok((
            Box::new(sound),
            TimeStretchHandle {
                shared,
                sample_rate: self.sample_rate,
            },
        ))
    }
}

/// The kira side of [`TimeStretchSoundData`].
struct TimeStretchSound {
    frames: Arc<[Frame]>,
    looping: bool,
    gain: f32,
    core: Wsola,
    /// False while the voice has stayed at rate 1 (direct reads).
    stretching: bool,
    hop: Vec<Frame>,
    hop_read: usize,
    /// The direct-read position.
    direct: i64,
    fade_left: usize,
    fade_len: usize,
    shared: Arc<StretchShared>,
    done: bool,
}

impl TimeStretchSound {
    fn past_end(&self, position: f64) -> bool {
        !self.looping && position >= (self.frames.len() + WSOLA_WINDOW) as f64
    }

    fn next_frame(&mut self) -> Frame {
        let rate = f32::from_bits(self.shared.rate.load(Ordering::Relaxed));
        let source = SliceSource {
            frames: &self.frames,
            looping: self.looping,
        };
        if !self.stretching {
            if rate == 1.0 {
                let frame = source.frame(self.direct);
                self.direct += 1;
                if self.looping {
                    self.direct = self.direct.rem_euclid(self.frames.len() as i64);
                }
                self.shared
                    .position
                    .store(self.direct.max(0) as u64, Ordering::Relaxed);
                if self.past_end(self.direct as f64) {
                    self.done = true;
                }
                return frame;
            }
            // Leaving rate 1: WSOLA picks up seamlessly where the direct reads stopped.
            self.core.restart(self.direct as f64, &source);
            self.stretching = true;
            self.hop_read = WSOLA_HOP;
        }
        if self.hop_read == WSOLA_HOP {
            let core = &mut self.core;
            core.next_hop(&source, rate, &mut self.hop);
            if self.looping {
                let len = self.frames.len() as f64;
                let wrap = (core.pos / len).floor() * len;
                core.pos -= wrap;
                core.prev -= wrap as i64;
            }
            let pos = core.pos;
            self.hop_read = 0;
            self.shared.hops.fetch_add(1, Ordering::Relaxed);
            self.shared.position.store(pos.max(0.0) as u64, Ordering::Relaxed);
            if self.past_end(pos) {
                self.done = true;
            }
        }
        let frame = self.hop[self.hop_read];
        self.hop_read += 1;
        frame
    }
}

impl Sound for TimeStretchSound {
    fn process(&mut self, out: &mut [Frame], _dt: f64, _info: &Info) {
        let stopping = self.shared.stop.load(Ordering::Relaxed);
        for slot in out.iter_mut() {
            if self.done {
                *slot = Frame::ZERO;
                continue;
            }
            let mut frame = self.next_frame() * self.gain;
            if stopping {
                frame *= self.fade_left as f32 / self.fade_len as f32;
                self.fade_left = self.fade_left.saturating_sub(1);
                if self.fade_left == 0 {
                    self.done = true;
                }
            }
            *slot = frame;
        }
        if self.done {
            self.shared.finished.store(true, Ordering::Relaxed);
        }
    }

    fn finished(&self) -> bool {
        self.done
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f32 = 48_000.0;

    fn tone(hz: f32, secs: f32) -> Vec<f32> {
        (0..(secs * RATE) as usize)
            .map(|i| (std::f32::consts::TAU * hz * i as f32 / RATE).sin() * 0.5)
            .collect()
    }

    /// Frequency by counting rising zero crossings with linear interpolation over the middle of
    /// the signal (away from the ends).
    fn frequency(samples: &[f32]) -> f32 {
        let from = samples.len() / 8;
        let to = samples.len() - samples.len() / 8;
        let mut crossings = Vec::new();
        for i in from..to {
            let (a, b) = (samples[i - 1], samples[i]);
            if a < 0.0 && b >= 0.0 {
                crossings.push((i - 1) as f32 + a / (a - b));
            }
        }
        let periods = (crossings.len() - 1) as f32;
        let span = crossings[crossings.len() - 1] - crossings[0];
        periods * RATE / span
    }

    #[test]
    fn stretch_keeps_the_pitch_and_scales_the_duration() {
        let input = tone(440.0, 2.0);
        for rate in [0.5f32, 0.75, 1.5, 2.0] {
            let out = time_stretch(&input, 1, rate).expect("stretch");
            let want_len = input.len() as f32 / rate;
            let ratio_err = (out.len() as f32 / want_len - 1.0).abs();
            assert!(
                ratio_err < 0.01,
                "rate {rate}: {} frames, wanted {want_len}",
                out.len()
            );
            let hz = frequency(&out);
            assert!((hz / 440.0 - 1.0).abs() < 0.01, "rate {rate}: {hz} Hz");
        }
    }

    #[test]
    fn rate_one_is_bit_identical() {
        let input = tone(440.0, 0.5);
        assert_eq!(time_stretch(&input, 1, 1.0).expect("same"), input);
        let stereo: Vec<f32> = input.iter().flat_map(|s| [*s, -*s]).collect();
        assert_eq!(time_stretch(&stereo, 2, 1.0).expect("same"), stereo);
    }

    #[test]
    fn stereo_keeps_both_channels() {
        let left = tone(440.0, 1.0);
        let right = tone(660.0, 1.0);
        let stereo: Vec<f32> = left.iter().zip(&right).flat_map(|(l, r)| [*l, *r]).collect();
        let out = time_stretch(&stereo, 2, 1.5).expect("stretch");
        let l: Vec<f32> = out.iter().step_by(2).copied().collect();
        let r: Vec<f32> = out.iter().skip(1).step_by(2).copied().collect();
        assert!((frequency(&l) / 440.0 - 1.0).abs() < 0.01);
        assert!((frequency(&r) / 660.0 - 1.0).abs() < 0.015);
    }

    #[test]
    fn bad_input_is_refused() {
        assert_eq!(
            time_stretch(&[0.0; 4], 1, 0.25).map(|_| ()),
            Err(TimeStretchError::Rate(0.25))
        );
        assert_eq!(
            time_stretch(&[0.0; 4], 1, f32::NAN)
                .map(|_| ())
                .map_err(|e| e.code()),
            Err("stretch_rate")
        );
        assert_eq!(
            time_stretch(&[], 1, 1.5).map(|_| ()),
            Err(TimeStretchError::Empty)
        );
        assert_eq!(
            time_stretch(&[0.0; 3], 2, 1.5).map(|_| ()),
            Err(TimeStretchError::Channels(2))
        );
    }

    /// An in-memory stereo sound of `left`/`right` at 48 kHz.
    fn stereo_asset(left: &[f32], right: &[f32]) -> crate::SoundAsset {
        let frames: Arc<[Frame]> = left
            .iter()
            .zip(right)
            .map(|(l, r)| Frame { left: *l, right: *r })
            .collect();
        crate::SoundAsset::from_static_data(kira::sound::static_sound::StaticSoundData {
            sample_rate: 48_000,
            frames,
            slice: None,
            settings: kira::sound::static_sound::StaticSoundSettings::new(),
        })
    }

    fn channel(asset: &crate::SoundAsset, right: bool) -> Vec<f32> {
        let (frames, _) = asset.static_frames().expect("static");
        frames
            .iter()
            .map(|f| if right { f.right } else { f.left })
            .collect()
    }

    /// Phase (radians) of the `hz` component of `x` over its middle half.
    fn phase(x: &[f32], hz: f32) -> f32 {
        let (from, to) = (x.len() / 4, x.len() * 3 / 4);
        let (mut c, mut s) = (0.0f64, 0.0f64);
        for (n, v) in x.iter().enumerate().take(to).skip(from) {
            let w = f64::from(std::f32::consts::TAU * hz) * n as f64 / f64::from(RATE);
            c += f64::from(*v) * w.cos();
            s += f64::from(*v) * w.sin();
        }
        c.atan2(s) as f32
    }

    /// The largest sample-to-sample step of `x`.
    fn max_step(x: &[f32]) -> f32 {
        x.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f32::max)
    }

    /// A speech-like fixture: a glottal-like harmonic series (8 harmonics at 1/k) whose
    /// fundamental glides 110 → 170 Hz, under a 4 Hz syllable envelope.
    fn speech_like(secs: f32) -> Vec<f32> {
        let n = (secs * RATE) as usize;
        let mut phase = 0.0f32;
        (0..n)
            .map(|i| {
                let t = i as f32 / RATE;
                let f0 = 110.0 + 60.0 * t / secs;
                phase += std::f32::consts::TAU * f0 / RATE;
                let voice: f32 = (1..=8).map(|k| (phase * k as f32).sin() / k as f32).sum();
                let envelope = 0.5 - 0.5 * (std::f32::consts::TAU * 4.0 * t).cos();
                voice * envelope * 0.25
            })
            .collect()
    }

    /// `stretch_asset` (ADR-3197): a 440 Hz tone keeps its pitch at 0.5, 0.75, 1.5 and 2 within
    /// 1 % and lasts `range / rate` (to the frame); rate 1 is the range bit for bit; the range is
    /// clipped to the sound; bad input is refused with its code.
    #[test]
    fn stretch_asset_keeps_pitch_scales_duration_and_passes_rate_one_through() {
        let tone = tone(440.0, 2.0);
        let asset = stereo_asset(&tone, &tone);
        for rate in [0.5f32, 0.75, 1.5, 2.0] {
            let out = stretch_asset(&asset, 0.25, 1.5, rate).expect("stretch");
            let left = channel(&out, false);
            let want = (1.5 * 48_000.0 / f64::from(rate)).round() as usize;
            assert_eq!(left.len(), want, "rate {rate}");
            let hz = frequency(&left);
            assert!((hz / 440.0 - 1.0).abs() < 0.01, "rate {rate}: {hz} Hz");
            assert_eq!(out.sample_rate(), 48_000);
        }
        let same = stretch_asset(&asset, 0.25, 1.5, 1.0).expect("rate 1");
        assert_eq!(channel(&same, false), tone[12_000..84_000].to_vec());
        // Past the end: clipped to the sound.
        let tail = stretch_asset(&asset, 1.5, 10.0, 2.0).expect("tail");
        assert_eq!(channel(&tail, false).len(), 12_000);
        let code = |r: Result<crate::SoundAsset, crate::AudioError>| match r {
            Err(crate::AudioError::Invalid(text)) => text.split(':').next().unwrap_or("").to_string(),
            other => format!("{:?}", other.map(|a| a.duration_secs())),
        };
        assert_eq!(code(stretch_asset(&asset, 0.0, 1.0, 3.0)), "stretch_rate");
        assert_eq!(code(stretch_asset(&asset, 0.0, 1.0, f32::NAN)), "stretch_rate");
        assert_eq!(code(stretch_asset(&asset, 5.0, 1.0, 1.5)), "stretch_empty");
        assert_eq!(code(stretch_asset(&asset, 0.5, 0.0, 1.5)), "stretch_empty");
    }

    /// Stereo phase holds through `stretch_asset`: both channels take the same segment, so a
    /// 90° offset between left and right stays within 3°.
    #[test]
    fn stretch_asset_keeps_the_stereo_phase() {
        let left = tone(440.0, 2.0);
        let right: Vec<f32> = (0..left.len())
            .map(|i| {
                (std::f32::consts::TAU * 440.0 * i as f32 / RATE + std::f32::consts::FRAC_PI_2).sin() * 0.5
            })
            .collect();
        let asset = stereo_asset(&left, &right);
        for rate in [0.5f32, 0.75, 1.5, 2.0] {
            let out = stretch_asset(&asset, 0.0, 2.0, rate).expect("stretch");
            let diff = phase(&channel(&out, true), 440.0) - phase(&channel(&out, false), 440.0);
            let diff = (diff + std::f32::consts::PI).rem_euclid(std::f32::consts::TAU) - std::f32::consts::PI;
            assert!(
                (diff - std::f32::consts::FRAC_PI_2).abs() < 3.0f32.to_radians(),
                "rate {rate}: {} degrees",
                diff.to_degrees()
            );
        }
    }

    /// No clicks on a speech-like fixture: the largest sample-to-sample step of the stretch stays
    /// within 1.5 times the input's largest step at every rate.
    #[test]
    fn stretch_asset_adds_no_clicks_to_speech() {
        let speech = speech_like(2.0);
        let bound = 1.5 * max_step(&speech);
        let asset = stereo_asset(&speech, &speech);
        for rate in [0.5f32, 0.75, 1.5, 2.0] {
            let out = stretch_asset(&asset, 0.0, 2.0, rate).expect("stretch");
            let step = max_step(&channel(&out, false));
            assert!(step <= bound, "rate {rate}: step {step} over {bound}");
        }
    }

    /// The per-voice cost behind ADR-3197's choice (run by hand, release build):
    /// `cargo test -p regia_audio --release --lib stretch_cost -- --ignored --nocapture`.
    #[test]
    #[ignore = "measurement, prints the WSOLA cost per second of output"]
    fn stretch_cost() {
        let speech = speech_like(10.0);
        let asset = stereo_asset(&speech, &speech);
        for rate in [0.5f32, 0.75, 1.5, 2.0] {
            let started = std::time::Instant::now();
            let out = stretch_asset(&asset, 0.0, 10.0, rate).expect("stretch");
            let took = started.elapsed().as_secs_f64();
            let secs = out.duration_secs();
            println!(
                "rate {rate}: {secs:.2} s of output in {:.1} ms = {:.2} ms per output second ({:.0}x real time)",
                took * 1e3,
                took * 1e3 / secs,
                secs / took
            );
        }
    }

    #[test]
    fn a_voice_matches_the_offline_stretch_in_pitch_and_passes_rate_one_through() {
        let input = tone(440.0, 1.0);
        let frames: Arc<[Frame]> = input.iter().map(|s| Frame { left: *s, right: *s }).collect();
        let info = kira::info::MockInfoBuilder::new().build();
        // Rate 1: the voice's frames are the input's.
        let data = TimeStretchSoundData::new(Arc::clone(&frames), 48_000, 1.0, false, 1.0).expect("data");
        let (mut sound, handle) = data.into_sound().expect("sound");
        let mut out = vec![Frame::ZERO; 4_800];
        sound.process(&mut out, 1.0 / 48_000.0, &info);
        assert!(out.iter().zip(frames.iter()).all(|(a, b)| a == b));
        // Then 0.75: the pitch holds.
        handle.set_rate(0.75).expect("rate");
        let mut stretched = vec![Frame::ZERO; 48_000];
        sound.process(&mut stretched, 1.0 / 48_000.0, &info);
        let mono: Vec<f32> = stretched.iter().map(|f| f.left).collect();
        assert!(
            (frequency(&mono) / 440.0 - 1.0).abs() < 0.01,
            "{}",
            frequency(&mono)
        );
        assert!(handle.hops() > 0);
        // The input advanced about 0.75 × 48 000 frames past the 4 800 read directly.
        let advanced = handle.position_secs() * 48_000.0 - 4_800.0;
        assert!((advanced / 36_000.0 - 1.0).abs() < 0.03, "advanced {advanced}");
        handle.stop();
        let mut tail = vec![Frame::ZERO; 1_024];
        sound.process(&mut tail, 1.0 / 48_000.0, &info);
        assert!(sound.finished() && handle.finished());
    }
}
