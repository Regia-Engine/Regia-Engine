// SPDX-License-Identifier: MPL-2.0
//! The capture processing chain (`docs/specs/59-voice-chat.md` § 2.7, ADR-2608): high-pass, echo
//! cancellation, noise suppression and automatic gain, in that order, between format conversion
//! and the encoder.
//!
//! Order: the high-pass first (rumble and DC would waste the adaptive filter's effort); echo
//! cancellation before noise suppression, because the echo path is linear and the suppressor is
//! not (a gain that changes per frame would look like a moving echo path); gain last, so it
//! levels the cleaned voice and not echo or noise.
//!
//! Every stage is off by default except the high-pass, is allocation-free after construction,
//! and is bypassed (its input passes on, never silence) when it produces a non-finite sample. Each
//! stage keeps a deterministic cost counter (FFTs, multiply-accumulates) and the measured time
//! per frame for `voice_chat.processing`; tests assert the counters, never the time.

use super::aec::{AecStats, EchoCanceller};
use super::agc::{AgcStats, AutoGain};
use super::capture::HighPass;
use super::denoise::{NoiseSuppressor, NoiseSuppressorStats};
use super::settings::{NoiseSuppression, ProcessingSettings};
use std::collections::VecDeque;
use std::time::Instant;

/// Cut-off of the always-on high-pass, Hz.
pub const HIGH_PASS_HZ: f32 = 90.0;
/// Most far-end reference kept for echo cancellation, in 20 ms frames. Older samples are dropped
/// (and counted), so the reference can never fall behind the capture: a tick without a capture
/// frame, or a host pushing in uneven chunks, costs at most one frame of alignment, which the
/// 128 ms filter tail absorbs.
pub const REFERENCE_MAX_FRAMES: usize = 2;

/// Stage names, as `voice_chat.status` `effective_processing` and `voice_chat.processing` report
/// them, in chain order.
pub const STAGE_NAMES: [&str; 4] = ["high_pass", "echo_cancel", "noise_suppression_light", "agc"];

/// Where the echo canceller's far-end reference comes from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EchoReference {
    /// The session's own voice chat playout (what remote speakers say), fed every frame while an
    /// audio output is attached. The default.
    #[default]
    Playout,
    /// The host pushes the final game mix through `VoiceChatSession::push_echo_reference`, one
    /// 20 ms frame per tick at the capture rate (a bus-tree tap).
    External,
}

/// Cost and outcome counters of one stage.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StageStats {
    /// Frames the stage ran on.
    pub frames: u64,
    /// Frames the stage was bypassed after a non-finite result.
    pub bypassed: u64,
    /// Total measured time, nanoseconds (diagnostic only; never asserted).
    pub total_ns: u64,
    /// Longest measured frame, nanoseconds (diagnostic only).
    pub max_ns: u64,
}

impl StageStats {
    /// Mean measured microseconds per frame (0 before the first frame).
    #[must_use]
    pub fn mean_us(&self) -> f64 {
        if self.frames == 0 {
            0.0
        } else {
            self.total_ns as f64 / self.frames as f64 / 1000.0
        }
    }

    fn record(&mut self, start: Instant) {
        let ns = u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX);
        self.frames += 1;
        self.total_ns = self.total_ns.saturating_add(ns);
        self.max_ns = self.max_ns.max(ns);
    }
}

/// Everything `voice_chat.processing` reports.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProcessingStats {
    /// Per stage in chain order (see [`STAGE_NAMES`]).
    pub stages: [StageStats; 4],
    /// Echo canceller counters (zero when it never ran).
    pub aec: AecStats,
    /// Noise suppressor counters.
    pub noise: NoiseSuppressorStats,
    /// Automatic gain counters.
    pub agc: AgcStats,
    /// Far-end reference frames received.
    pub reference_frames: u64,
    /// Capture frames that found too little reference (padded with silence).
    pub reference_underruns: u64,
    /// Reference samples dropped because the ring was full.
    pub reference_dropped: u64,
    /// Non-finite input samples replaced by silence before the chain (they would poison the
    /// filters' state for good).
    pub nonfinite_samples: u64,
    /// Frames of the wrong length (not 20 ms at the chain's rate) passed through unprocessed.
    pub frame_mismatches: u64,
}

/// The capture processing chain of one session.
#[derive(Debug)]
pub struct ProcessingChain {
    settings: ProcessingSettings,
    rate: u32,
    frame_samples: usize,
    high_pass: HighPass,
    aec: Option<Box<EchoCanceller>>,
    noise: Option<Box<NoiseSuppressor>>,
    agc: Option<AutoGain>,
    reference_source: EchoReference,
    reference: VecDeque<f32>,
    reference_cap: usize,
    reference_attached: bool,
    far: Vec<f32>,
    backup: Vec<f32>,
    stats: ProcessingStats,
    warned_no_reference: bool,
    warned_mismatch: bool,
}

impl ProcessingChain {
    /// A chain for `sample_rate` with 20 ms frames.
    #[must_use]
    pub fn new(settings: ProcessingSettings, sample_rate: u32) -> Self {
        let frame_samples = (sample_rate / 50) as usize;
        let reference_cap = REFERENCE_MAX_FRAMES * frame_samples;
        let mut chain = Self {
            settings: ProcessingSettings::default(),
            rate: sample_rate,
            frame_samples,
            high_pass: HighPass::new(HIGH_PASS_HZ, sample_rate),
            aec: None,
            noise: None,
            agc: None,
            reference_source: EchoReference::Playout,
            reference: VecDeque::with_capacity(reference_cap),
            reference_cap,
            reference_attached: false,
            far: vec![0.0; frame_samples],
            backup: vec![0.0; frame_samples],
            stats: ProcessingStats::default(),
            warned_no_reference: false,
            warned_mismatch: false,
        };
        chain.configure(settings);
        chain
    }

    /// Applies new settings: a stage turned on starts fresh, a stage turned off is dropped, a stage
    /// that stays on keeps what it learned.
    pub fn configure(&mut self, settings: ProcessingSettings) {
        if settings.echo_cancel != self.aec.is_some() {
            self.aec = settings
                .echo_cancel
                .then(|| Box::new(EchoCanceller::new(self.rate)));
            self.reference.clear();
            self.warned_no_reference = false;
        }
        let ns_on = settings.noise_suppression != NoiseSuppression::Off;
        if ns_on != self.noise.is_some() {
            self.noise = ns_on.then(|| Box::new(NoiseSuppressor::new(self.rate)));
        }
        if settings.agc != self.agc.is_some() {
            self.agc = settings.agc.then(AutoGain::new);
        }
        self.settings = settings;
    }

    /// The requested settings.
    #[must_use]
    pub fn settings(&self) -> ProcessingSettings {
        self.settings
    }

    /// The sample rate the chain runs at.
    #[must_use]
    pub fn sample_rate(&self) -> u32 {
        self.rate
    }

    /// Chooses the echo reference source; switching clears the reference ring.
    pub fn set_reference_source(&mut self, source: EchoReference) {
        if source != self.reference_source {
            self.reference_source = source;
            self.reference.clear();
        }
    }

    /// The echo reference source.
    #[must_use]
    pub fn reference_source(&self) -> EchoReference {
        self.reference_source
    }

    /// Whether a reference is attached: the session sets this when an audio output turns its
    /// output tap on (source `Playout`); an `External` source counts once it has pushed a frame.
    pub fn set_reference_attached(&mut self, attached: bool) {
        self.reference_attached = attached;
        if !attached {
            self.reference.clear();
        }
    }

    /// Whether a far-end reference is attached (an output tap, or a pushed external frame).
    #[must_use]
    pub fn reference_attached(&self) -> bool {
        self.reference_attached
    }

    /// Whether echo cancellation can run: it is on and has a reference (spec § 2.7: a missing
    /// reference turns it off with a warning).
    #[must_use]
    pub fn echo_cancel_effective(&self) -> bool {
        self.aec.is_some() && self.reference_attached
    }

    /// Appends far-end reference samples at the chain's rate. Older samples beyond
    /// [`REFERENCE_MAX_FRAMES`] are dropped and counted.
    pub fn push_reference(&mut self, samples: &[f32]) {
        if self.aec.is_none() {
            return;
        }
        if self.reference_source == EchoReference::External {
            self.reference_attached = true;
        }
        self.stats.reference_frames += 1;
        for s in samples {
            if self.reference.len() == self.reference_cap {
                self.reference.pop_front();
                self.stats.reference_dropped += 1;
            }
            self.reference.push_back(if s.is_finite() { *s } else { 0.0 });
        }
    }

    /// The stages that run on the next frame, in chain order. `deterministic` bypasses the
    /// high-pass (spec § 7: the chain is bypassed unless a test names a stage; the other stages
    /// only run when named).
    #[must_use]
    pub fn effective(&self, deterministic: bool) -> Vec<&'static str> {
        let mut out = Vec::new();
        if !deterministic {
            out.push(STAGE_NAMES[0]);
        }
        if self.echo_cancel_effective() {
            out.push(STAGE_NAMES[1]);
        }
        if self.noise.is_some() {
            out.push(STAGE_NAMES[2]);
        }
        if self.agc.is_some() {
            out.push(STAGE_NAMES[3]);
        }
        out
    }

    /// The counters, with each stage's own counters folded in.
    #[must_use]
    pub fn stats(&self) -> ProcessingStats {
        let mut s = self.stats.clone();
        if let Some(a) = &self.aec {
            s.aec = a.stats();
        }
        if let Some(n) = &self.noise {
            s.noise = n.stats();
        }
        if let Some(g) = &self.agc {
            s.agc = g.stats();
        }
        s
    }

    /// Clears every counter (what the stages learned stays).
    pub fn reset_stats(&mut self) {
        self.stats = ProcessingStats::default();
        if let Some(a) = &mut self.aec {
            a.reset_stats();
        }
        if let Some(n) = &mut self.noise {
            n.reset_stats();
        }
        if let Some(g) = &mut self.agc {
            g.reset_stats();
        }
    }

    /// Replaces non-finite samples with silence and counts them.
    fn sanitize(&mut self, frame: &mut [f32]) {
        for s in frame.iter_mut().filter(|s| !s.is_finite()) {
            *s = 0.0;
            self.stats.nonfinite_samples += 1;
        }
    }

    /// The high-pass alone, before the user's input gain (as the baseline did). Non-finite input
    /// samples become silence first, so one bad sample cannot poison the filter state.
    pub fn high_pass(&mut self, frame: &mut [f32], deterministic: bool) {
        self.sanitize(frame);
        if deterministic {
            return;
        }
        let start = Instant::now();
        self.high_pass.process(frame);
        self.stats.stages[0].record(start);
    }

    /// Echo cancellation, noise suppression and automatic gain, in that order, on one frame.
    pub fn process(&mut self, frame: &mut [f32]) {
        let _span = tracing::trace_span!("voice_chat_capture").entered();
        if frame.len() != self.frame_samples {
            // Keep the reference moving with the capture even when this frame cannot be used.
            let drop = self.reference.len().min(self.frame_samples);
            self.reference.drain(..drop);
            self.stats.frame_mismatches += 1;
            if !self.warned_mismatch {
                self.warned_mismatch = true;
                tracing::warn!(
                    target: "regia_audio",
                    "voice chat processing got a {}-sample frame at {} Hz (expected {}); passed through",
                    frame.len(),
                    self.rate,
                    self.frame_samples
                );
            }
            return;
        }
        self.sanitize(frame);
        // Far-end single talk: the far end plays and no double talk was seen in this frame. The
        // gain stage holds (never boosts) then, so it cannot amplify residual echo.
        let mut far_single_talk = false;
        if let Some(aec) = self.aec.as_deref_mut() {
            if self.reference_attached {
                let _span = tracing::trace_span!("voice_chat_aec").entered();
                let have = self.reference.len().min(self.frame_samples);
                if have < self.frame_samples {
                    self.stats.reference_underruns += 1;
                }
                for (dst, src) in self.far.iter_mut().zip(self.reference.drain(..have)) {
                    *dst = src;
                }
                self.far[have..].fill(0.0);
                self.backup.copy_from_slice(frame);
                let before = aec.stats();
                let start = Instant::now();
                aec.process(frame, &self.far);
                self.stats.stages[1].record(start);
                let after = aec.stats();
                far_single_talk = after.adapted_blocks > before.adapted_blocks
                    && after.double_talk_blocks == before.double_talk_blocks;
                if frame.iter().any(|s| !s.is_finite()) {
                    frame.copy_from_slice(&self.backup);
                    self.stats.stages[1].bypassed += 1;
                    // A non-finite filter never recovers: start again.
                    *aec = EchoCanceller::new(self.rate);
                }
            } else if !self.warned_no_reference {
                self.warned_no_reference = true;
                tracing::warn!(
                    target: "regia_audio",
                    "voice chat echo cancellation is on but no playback reference is attached; it stays off until one is"
                );
            }
        }
        if let Some(ns) = self.noise.as_deref_mut() {
            let _span = tracing::trace_span!("voice_chat_denoise").entered();
            self.backup.copy_from_slice(frame);
            let start = Instant::now();
            ns.process(frame);
            self.stats.stages[2].record(start);
            if frame.iter().any(|s| !s.is_finite()) {
                frame.copy_from_slice(&self.backup);
                self.stats.stages[2].bypassed += 1;
                *ns = NoiseSuppressor::new(self.rate);
            }
        }
        if let Some(agc) = self.agc.as_mut() {
            self.backup.copy_from_slice(frame);
            let start = Instant::now();
            agc.process_held(frame, far_single_talk);
            self.stats.stages[3].record(start);
            if frame.iter().any(|s| !s.is_finite()) {
                frame.copy_from_slice(&self.backup);
                self.stats.stages[3].bypassed += 1;
                *agc = AutoGain::new();
            }
        }
    }
}

/// Adds `src` (at `src_rate`) into `dst` (one frame at `dst_rate`). Rates that differ are
/// converted: downsampling averages the source samples each output sample covers (a box filter, so
/// the band the capture cannot hold does not fold into the reference as uncorrelated input),
/// upsampling interpolates linearly. Used to fold remote speakers' playout into the echo
/// reference.
pub fn mix_into(dst: &mut [f32], src: &[f32], src_rate: u32, dst_rate: u32) {
    if src.is_empty() || dst.is_empty() {
        return;
    }
    if src_rate == dst_rate {
        for (d, s) in dst.iter_mut().zip(src) {
            *d += s;
        }
        return;
    }
    let ratio = f64::from(src_rate) / f64::from(dst_rate);
    if ratio > 1.0 {
        for (i, d) in dst.iter_mut().enumerate() {
            let from = ((i as f64 * ratio) as usize).min(src.len() - 1);
            let to = (((i + 1) as f64 * ratio) as usize).clamp(from + 1, src.len());
            let window = &src[from..to];
            *d += window.iter().sum::<f32>() / window.len() as f32;
        }
        return;
    }
    let last = src.len() - 1;
    for (i, d) in dst.iter_mut().enumerate() {
        let pos = i as f64 * ratio;
        let j = (pos.floor() as usize).min(last);
        let frac = (pos - j as f64) as f32;
        let a = src[j];
        let b = src[(j + 1).min(last)];
        *d += a + (b - a) * frac;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice_chat::dsp_fixtures::{
        convolve, energy, room_impulse_response, speech_like, white_noise,
    };

    fn all_on() -> ProcessingSettings {
        ProcessingSettings {
            noise_suppression: NoiseSuppression::Light,
            echo_cancel: true,
            agc: true,
        }
    }

    /// Defaults: every optional stage off, only the high-pass reported, and `process` leaves the
    /// frame bit-identical.
    #[test]
    fn defaults_run_only_the_high_pass() {
        let mut chain = ProcessingChain::new(ProcessingSettings::default(), 16_000);
        assert_eq!(chain.effective(false), vec!["high_pass"]);
        assert!(chain.effective(true).is_empty());
        let input = white_noise(320, 1, -30.0);
        let mut frame = input.clone();
        chain.process(&mut frame);
        assert_eq!(frame, input);
        let s = chain.stats();
        assert!(s.stages[1..].iter().all(|st| st.frames == 0));
    }

    /// Echo cancellation without a reference stays off (and says so); with one attached it runs.
    #[test]
    fn echo_cancel_needs_a_reference() {
        let mut chain = ProcessingChain::new(
            ProcessingSettings {
                echo_cancel: true,
                ..ProcessingSettings::default()
            },
            16_000,
        );
        assert_eq!(chain.effective(false), vec!["high_pass"]);
        let mut frame = white_noise(320, 2, -30.0);
        chain.process(&mut frame);
        assert_eq!(chain.stats().stages[1].frames, 0);
        chain.set_reference_attached(true);
        assert_eq!(chain.effective(false), vec!["high_pass", "echo_cancel"]);
        chain.process(&mut frame);
        let s = chain.stats();
        assert_eq!(s.stages[1].frames, 1);
        assert_eq!(s.reference_underruns, 1);
    }

    /// `strong` runs the light suppressor (the network is not built) and reports it as light.
    #[test]
    fn strong_reports_as_light() {
        let chain = ProcessingChain::new(
            ProcessingSettings {
                noise_suppression: NoiseSuppression::Strong,
                ..ProcessingSettings::default()
            },
            24_000,
        );
        assert_eq!(
            chain.effective(false),
            vec!["high_pass", "noise_suppression_light"]
        );
    }

    /// The whole chain in order on a far-end echo plus near-end noise: the echo is removed
    /// before the suppressor and the gain stage sees the cleaned signal; every stage counted
    /// every frame; nothing non-finite.
    #[test]
    fn full_chain_runs_in_order() {
        let rate = 16_000u32;
        let frame = 320;
        let n = rate as usize * 4;
        let far = speech_like(n, rate, 1, -20.0);
        let echo = convolve(&far, &room_impulse_response(rate, 1));
        let noise = white_noise(n, 2, -50.0);
        let mic: Vec<f32> = echo.iter().zip(&noise).map(|(e, v)| e + v).collect();
        let mut chain = ProcessingChain::new(all_on(), rate);
        chain.set_reference_attached(true);
        assert_eq!(
            chain.effective(false),
            vec!["high_pass", "echo_cancel", "noise_suppression_light", "agc"]
        );
        let mut out = Vec::with_capacity(n);
        for (m, f) in mic.chunks(frame).zip(far.chunks(frame)) {
            chain.push_reference(f);
            let mut buf = m.to_vec();
            chain.process(&mut buf);
            out.extend_from_slice(&buf);
        }
        let s = chain.stats();
        let frames = (n / frame) as u64;
        for st in &s.stages[1..] {
            assert_eq!(st.frames, frames);
            assert_eq!(st.bypassed, 0);
        }
        assert_eq!(s.reference_underruns, 0);
        assert!(out.iter().all(|v| v.is_finite()));
        // Far-end-only input: the output in the last second is far below the echo even after
        // the gain stage (which never boosts the gated residual).
        let from = 3 * rate as usize;
        let drop = 10.0 * (energy(&out[from..]) / energy(&echo[from..])).log10();
        assert!(
            drop <= -20.0,
            "echo only {drop:.1} dB down after the chain, {s:?}"
        );
    }

    /// A stage turned off and on again starts fresh; one that stays on keeps its state.
    #[test]
    fn configure_keeps_or_rebuilds_stages() {
        let mut chain = ProcessingChain::new(all_on(), 16_000);
        chain.set_reference_attached(true);
        let mut f = white_noise(320, 3, -30.0);
        chain.push_reference(&f);
        chain.process(&mut f);
        assert_eq!(chain.stats().aec.blocks, 2);
        chain.configure(ProcessingSettings {
            agc: false,
            ..all_on()
        });
        assert_eq!(chain.stats().aec.blocks, 2);
        chain.configure(ProcessingSettings {
            echo_cancel: false,
            ..all_on()
        });
        chain.configure(all_on());
        assert_eq!(chain.stats().aec.blocks, 0);
    }

    /// Resampled reference folding keeps a constant constant and sums.
    #[test]
    fn mix_into_resamples_linearly() {
        let mut dst = vec![0.0f32; 320];
        mix_into(&mut dst, &[0.5; 960], 48_000, 16_000);
        mix_into(&mut dst, &[0.25; 320], 16_000, 16_000);
        assert!(dst.iter().all(|v| (v - 0.75).abs() < 1e-6));
    }

    /// Per-stage cost of one 20 ms frame at every profile rate, from the deterministic counters
    /// (asserted) and the measured time (printed only; run with `--nocapture` for the ADR table).
    #[test]
    fn per_stage_cost_per_frame() {
        for rate in [16_000u32, 24_000, 48_000] {
            let frame = (rate / 50) as usize;
            let frames = 100;
            let far = speech_like(frame * frames, rate, 4, -20.0);
            let mut chain = ProcessingChain::new(all_on(), rate);
            chain.set_reference_attached(true);
            for f in far.chunks(frame) {
                chain.push_reference(f);
                let mut buf = f.to_vec();
                chain.high_pass(&mut buf, false);
                chain.process(&mut buf);
            }
            let s = chain.stats();
            // Six FFTs per adapting 10 ms block, three while the far end pauses.
            assert!(
                s.aec.ffts <= 12 * frames as u64 && s.aec.ffts >= 6 * frames as u64,
                "{rate}: {:?}",
                s.aec
            );
            assert_eq!(s.noise.ffts, 4 * frames as u64);
            println!(
                "voice_chat processing {rate} Hz per 20 ms frame: high_pass {:.1} us, echo_cancel {:.1} us \
                 ({} FFTs of {}, {} complex MACs), noise_suppression_light {:.1} us ({} FFTs of {}), agc {:.1} us",
                s.stages[0].mean_us(),
                s.stages[1].mean_us(),
                s.aec.ffts / frames as u64,
                frame,
                s.aec.complex_macs / frames as u64,
                s.stages[2].mean_us(),
                s.noise.ffts / frames as u64,
                frame,
                s.stages[3].mean_us(),
            );
        }
    }

    /// In a session, the voice playout of a remote speaker becomes the echo reference once an
    /// audio output attaches (one frame per tick, silence included), echo cancellation then runs
    /// on every captured frame and adapts while the speaker talks.
    #[test]
    fn session_playout_feeds_the_echo_reference() {
        use crate::voice_chat::{
            TestSource, VoiceChatSession, VoiceChatSettings, VoiceChatUserSettings, VoiceQuality,
        };
        let settings = VoiceChatSettings {
            deterministic: true,
            quality: VoiceQuality::Narrow,
            ..VoiceChatSettings::default()
        };
        let mut s = VoiceChatSession::new(settings, VoiceChatUserSettings::default()).expect("session");
        s.set_channel(1, Some(true), Some(true), None).expect("channel");
        s.set_fake_speaker(7, Some(TestSource::sine(440.0, -12.0)));
        s.set_test_source(Some(TestSource::noise(5, -50.0)));
        s.set_processing(ProcessingSettings {
            echo_cancel: true,
            ..ProcessingSettings::default()
        });
        for _ in 0..10 {
            s.tick(false);
        }
        // No output yet: no reference, the stage is not in effect and did not run.
        assert!(s.status().effective_processing.is_empty());
        assert_eq!(s.processing_stats().stages[1].frames, 0);
        s.set_output_tap(true);
        assert_eq!(s.status().effective_processing, vec!["echo_cancel"]);
        for _ in 0..50 {
            s.tick(false);
        }
        let st = s.processing_stats();
        assert_eq!(st.reference_frames, 50);
        assert_eq!(st.stages[1].frames, 50);
        assert!(st.reference_underruns <= 1, "{st:?}");
        assert!(
            st.aec.adapted_blocks > 0,
            "the speaker's playout never reached the reference: {st:?}"
        );
        // The project settings follow the change.
        assert!(s.settings().processing.echo_cancel);
    }

    /// The reference never falls behind: ticks with no capture frame, a burst of pushes, or a
    /// frame of the wrong length leave at most two frames queued, and the newest are kept.
    #[test]
    fn reference_stays_aligned() {
        let mut chain = ProcessingChain::new(
            ProcessingSettings {
                echo_cancel: true,
                ..ProcessingSettings::default()
            },
            16_000,
        );
        chain.set_reference_attached(true);
        for i in 0..25 {
            chain.push_reference(&[i as f32; 320]);
        }
        let s = chain.stats();
        assert_eq!(s.reference_dropped, 23 * 320);
        // The next capture frame is matched with the older of the two newest frames.
        let mut frame = vec![0.0f32; 320];
        chain.process(&mut frame);
        assert_eq!(chain.reference.len(), 320);
        assert!(chain.reference.iter().all(|v| *v == 24.0));
        // A wrong-length frame still consumes a frame of reference.
        let mut odd = vec![0.0f32; 100];
        chain.process(&mut odd);
        assert!(chain.reference.is_empty());
        assert_eq!(chain.stats().frame_mismatches, 1);
    }

    /// A NaN or infinite input sample becomes silence before the high-pass, so the chain keeps
    /// working on the next frames instead of rebuilding itself forever.
    #[test]
    fn nonfinite_input_is_silenced_once() {
        let mut chain = ProcessingChain::new(all_on(), 16_000);
        chain.set_reference_attached(true);
        let mut frame = white_noise(320, 1, -30.0);
        frame[10] = f32::NAN;
        frame[20] = f32::INFINITY;
        chain.high_pass(&mut frame, false);
        chain.process(&mut frame);
        assert!(frame.iter().all(|v| v.is_finite()));
        for _ in 0..10 {
            let mut next = white_noise(320, 2, -30.0);
            chain.push_reference(&[0.0; 320]);
            chain.high_pass(&mut next, false);
            chain.process(&mut next);
            assert!(next.iter().all(|v| v.is_finite()));
        }
        let s = chain.stats();
        assert_eq!(s.nonfinite_samples, 2);
        assert!(s.stages.iter().all(|st| st.bypassed == 0), "{s:?}");
    }

    /// Downsampling averages: a tone at the source's Nyquist folds to near silence instead of
    /// a full-scale alias.
    #[test]
    fn mix_into_filters_when_downsampling() {
        let src: Vec<f32> = (0..960).map(|i| if i % 2 == 0 { 1.0 } else { -1.0 }).collect();
        let mut dst = vec![0.0f32; 320];
        mix_into(&mut dst, &src, 48_000, 16_000);
        assert!(dst.iter().all(|v| v.abs() <= 1.0 / 3.0 + 1e-6), "{:?}", &dst[..4]);
    }
}
