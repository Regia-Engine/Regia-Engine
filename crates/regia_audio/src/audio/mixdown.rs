// SPDX-License-Identifier: MPL-2.0
//! Render-out mixdown (ADR-1520): the mixer rendered offline, block by block, on a caller's clock.
//!
//! [`Audio::begin_mixdown`] sets the current output aside (a device stream or the headless
//! renderer) and runs a fresh offline kira mixer at the requested sample rate, with the bus tree
//! rebuilt on it and the host's bus state (volumes, mutes, snapshot filters) re-applied, as an
//! output resume does. From then on the mixer only advances when [`Audio::render_mixdown`] is
//! called, by exactly the frames asked for, so a caller that derives those counts from a fixed
//! game clock gets the same samples on every run.
//!
//! Stems are taps on named buses. A regular bus is tapped after its fader: it gets a 0 dB send
//! into a send track of its own whose last effect copies the block and whose volume is silence,
//! so nothing of the tap reaches the mix (the mix is the same with and without stems, bit for
//! bit). kira applies a track's volume after its effects and feeds its sends after its volume, and
//! processes every sub-track before any send track in each block, so the tap is sample-aligned
//! with the mix. A send bus (no fader) is tapped at the end of its own effect chain.
//!
//! The cue picker restarts from its seed when a mixdown begins, so the random cue picks of a job do
//! not depend on how many cues the game played before it.
//!
//! [`Audio::end_mixdown`] drops the mixdown mixer, puts the old output back and rebuilds the bus
//! tree on it. Sounds playing when a mixdown begins or ends are not carried over (the same rule
//! as an output resume, `docs/specs/16-audio.md` § 4): autoplaying emitters restart on the next
//! sync, one-shots and music do not.

use std::ops::RangeInclusive;
use std::sync::{Arc, Mutex};

use kira::{
    Frame,
    effect::{Effect, EffectBuilder},
    info::Info,
};

use super::Audio;
use crate::backend::lock;
use crate::error::AudioError;

/// Sample rates a mixdown may run at, Hz.
pub const MIXDOWN_SAMPLE_RATES: RangeInclusive<u32> = 8_000..=192_000;

/// Most stems one mixdown may tap.
pub const MAX_MIXDOWN_STEMS: usize = 32;

/// What a render-out mixdown records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MixdownSettings {
    /// Sample rate the mixdown mixer runs at, Hz ([`MIXDOWN_SAMPLE_RATES`]).
    pub sample_rate: u32,
    /// Buses (regular or send) to record separately, in the order the blocks return them.
    pub stems: Vec<String>,
}

impl Default for MixdownSettings {
    fn default() -> Self {
        Self {
            sample_rate: 48_000,
            stems: Vec::new(),
        }
    }
}

/// One rendered block of a mixdown: interleaved stereo `f32` (left, right), `frames * 2` samples
/// each.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MixdownBlock {
    /// The master output: every bus and send bus after the master limiter.
    pub mix: Vec<f32>,
    /// One block per [`MixdownSettings::stems`] entry, in that order.
    pub stems: Vec<Vec<f32>>,
}

/// A stem tap: an effect that copies every block it sees into a shared buffer.
#[derive(Clone, Default)]
pub(crate) struct StemTap(Arc<Mutex<Vec<Frame>>>);

impl StemTap {
    /// Takes what the tap heard since the last take, as exactly `frames` interleaved stereo
    /// frames (silence pads a short take; a long one is cut).
    fn take_interleaved(&self, frames: usize) -> Vec<f32> {
        let heard = std::mem::take(&mut *lock(&self.0));
        let mut out = Vec::with_capacity(frames * 2);
        for frame in heard.iter().take(frames) {
            out.push(frame.left);
            out.push(frame.right);
        }
        out.resize(frames * 2, 0.0);
        out
    }

    fn clear(&self) {
        lock(&self.0).clear();
    }
}

impl EffectBuilder for StemTap {
    type Handle = ();

    fn build(self) -> (Box<dyn Effect>, Self::Handle) {
        (Box::new(StemTapEffect(self.0)), ())
    }
}

/// The kira side of a [`StemTap`]. It only ever runs on a mixdown's offline mixer, which renders
/// on the caller's thread, so the lock is never contended.
struct StemTapEffect(Arc<Mutex<Vec<Frame>>>);

impl Effect for StemTapEffect {
    fn process(&mut self, input: &mut [Frame], _dt: f64, _info: &Info) {
        lock(&self.0).extend_from_slice(input);
    }
}

/// A running mixdown.
pub(super) struct MixdownState {
    settings: MixdownSettings,
    taps: Vec<StemTap>,
}

impl Audio {
    /// Starts a render-out mixdown (see the module docs): validates `settings`, sets the current
    /// output aside and rebuilds the buses on a fresh offline mixer with the stem taps. Every
    /// voice playing now ends. `Err` (and nothing changes) when a mixdown already runs, the rate
    /// is outside [`MIXDOWN_SAMPLE_RATES`], a stem names no bus, or a stem is listed twice. A
    /// suspended output does not refuse it; it stays suspended through the take.
    pub fn begin_mixdown(&mut self, settings: &MixdownSettings) -> Result<(), AudioError> {
        self.check_mixdown(settings)?;
        let taps: Vec<(String, StemTap)> = settings
            .stems
            .iter()
            .map(|bus| (bus.clone(), StemTap::default()))
            .collect();
        self.release_voices();
        // + 1: the hidden zone-IR send bus (ADR-2721).
        let send_tracks = self.bus_layout.send_buses.len() + taps.len() + 1;
        if !self.backend.begin_mixdown(settings.sample_rate, send_tracks) {
            return Err(AudioError::Invalid("a mixdown is already running".into()));
        }
        let conv = self.convolution_context(settings.sample_rate);
        match self.backend.build_buses_tapped(&self.bus_layout, &taps, &conv) {
            Ok(buses) => self.buses = buses,
            Err(error) => {
                self.backend.end_mixdown();
                self.rebuild_after_output_swap();
                return Err(error);
            }
        }
        self.reapply_bus_state();
        self.reset_emitters_after_rebuild();
        self.streams.set_sample_rate(settings.sample_rate);
        // Cue variation in the job does not depend on how many cues played before it.
        self.rng = regia_core::math::Rng::new(super::CUE_SEED);
        // This frame's `Time::delta` was measured on the wall clock before the job took the clock,
        // so it must not enter the cue and music clocks the job's mix reads.
        self.skip_next_tick = true;
        self.mixdown = Some(MixdownState {
            settings: settings.clone(),
            taps: taps.into_iter().map(|(_, tap)| tap).collect(),
        });
        tracing::debug!(
            target: "regia_audio",
            sample_rate = settings.sample_rate,
            stems = settings.stems.len(),
            "render-out mixdown started"
        );
        Ok(())
    }

    /// Checks `settings` as [`Audio::begin_mixdown`] would, without changing anything.
    pub fn check_mixdown(&self, settings: &MixdownSettings) -> Result<(), AudioError> {
        if self.mixdown.is_some() || self.backend.in_mixdown() {
            return Err(AudioError::Invalid("a mixdown is already running".into()));
        }
        // A suspended output does not refuse a take: the mixdown renders on its own offline
        // mixer, a resume that arrives mid-take is deferred to `end_mixdown`, and `end_mixdown`
        // keeps a still-suspended output torn down (the app suspends whenever every window is
        // hidden and on a slow software adapter, where render-out jobs still run).
        validate_mixdown(settings, &self.bus_layout)
    }

    /// Renders the next `frames` stereo frames of the running mixdown: the master output and every
    /// stem. `Err` when no mixdown runs.
    pub fn render_mixdown(&mut self, frames: usize) -> Result<MixdownBlock, AudioError> {
        let Some(state) = self.mixdown.as_ref() else {
            return Err(AudioError::Invalid("no mixdown is running".into()));
        };
        let taps = state.taps.clone();
        // Anything a stray `render_offline` rendered since the last block is not this block.
        for tap in &taps {
            tap.clear();
        }
        let mix = self.render_offline(frames)?;
        let stems = taps.iter().map(|tap| tap.take_interleaved(frames)).collect();
        Ok(MixdownBlock { mix, stems })
    }

    /// Ends the running mixdown: drops its mixer, puts the output it set aside back and rebuilds
    /// the buses there. Every voice playing now ends. Returns `false` when no mixdown ran.
    ///
    /// Suspended mid-take, the restored output stays torn down (`output_suspended()` keeps
    /// telling the truth) and a resume that arrived during the take applies here; the next
    /// [`Audio::resume_output`] otherwise rebuilds the buses.
    pub fn end_mixdown(&mut self) -> bool {
        self.skip_next_tick = false;
        if self.mixdown.take().is_none() {
            return false;
        }
        self.release_voices();
        self.backend.end_mixdown();
        if self.suspended {
            // The take stashed a (now torn-down) suspended output: restoring it live would
            // leave `output_suspended()` true against a running stream, and the next resume
            // would rebuild what was never closed. Keep it torn down.
            self.backend.suspend();
        }
        let deferred = std::mem::take(&mut self.resume_after_mixdown) && self.suspended;
        if deferred {
            if !self.resume_output() {
                tracing::warn!(
                    target: "regia_audio",
                    "the resume deferred by a mixdown could not re-open the output; the next resume retries"
                );
            }
        } else if !self.suspended {
            self.rebuild_after_output_swap();
        }
        self.streams.set_sample_rate(self.output_sample_rate);
        tracing::debug!(target: "regia_audio", "render-out mixdown ended");
        true
    }

    /// The running mixdown's settings, or `None`.
    pub fn mixdown(&self) -> Option<&MixdownSettings> {
        self.mixdown.as_ref().map(|state| &state.settings)
    }

    /// True while a render-out mixdown runs.
    pub fn mixdown_active(&self) -> bool {
        self.mixdown.is_some()
    }

    /// Rebuilds the bus tree on the output now in place and re-applies the host's bus state.
    fn rebuild_after_output_swap(&mut self) {
        let conv = self.convolution_context(self.output_sample_rate);
        match self.backend.build_buses(&self.bus_layout, &conv) {
            Ok(buses) => self.buses = buses,
            Err(error) => tracing::error!(target: "regia_audio", "bus rebuild failed: {error}"),
        }
        self.reapply_bus_state();
        self.reset_emitters_after_rebuild();
    }
}

/// Checks `settings` against the bus `layout`.
fn validate_mixdown(settings: &MixdownSettings, layout: &crate::bus::BusLayout) -> Result<(), AudioError> {
    if !MIXDOWN_SAMPLE_RATES.contains(&settings.sample_rate) {
        return Err(AudioError::Invalid(format!(
            "mixdown sample rate {} Hz is outside {}..={} Hz",
            settings.sample_rate,
            MIXDOWN_SAMPLE_RATES.start(),
            MIXDOWN_SAMPLE_RATES.end()
        )));
    }
    if settings.stems.len() > MAX_MIXDOWN_STEMS {
        return Err(AudioError::Invalid(format!(
            "{} stems; a mixdown taps at most {MAX_MIXDOWN_STEMS}",
            settings.stems.len()
        )));
    }
    for (i, stem) in settings.stems.iter().enumerate() {
        let known =
            layout.buses.iter().any(|b| b.name == *stem) || layout.send_buses.iter().any(|s| s.name == *stem);
        if !known {
            return Err(AudioError::UnknownBus(stem.clone()));
        }
        if settings.stems[..i].contains(stem) {
            return Err(AudioError::Invalid(format!("stem `{stem}` is listed twice")));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clip::SoundClip;
    use crate::settings::AudioSettings;
    use crate::sound::PlaybackParams;

    fn headless() -> Audio {
        Audio::new(&AudioSettings {
            headless: true,
            ..AudioSettings::default()
        })
    }

    /// A 0.1 s, 440 Hz tone at amplitude 0.5, mono 48 kHz.
    fn tone() -> crate::sound::SoundAsset {
        let samples: Vec<f32> = (0..4_800)
            .map(|i| ((i as f32 / 48_000.0) * 440.0 * std::f32::consts::TAU).sin() * 0.5)
            .collect();
        crate::sound::SoundAsset::from_clip(&SoundClip::new(samples, 48_000).expect("tone"))
    }

    fn on_bus(bus: &str) -> PlaybackParams {
        PlaybackParams {
            bus: Some(bus.into()),
            ..PlaybackParams::default()
        }
    }

    fn peak(samples: &[f32]) -> f32 {
        samples.iter().fold(0.0_f32, |acc, s| acc.max(s.abs()))
    }

    /// Plays the tone on `bus` in a fresh mixdown and renders `blocks` blocks of 800 frames.
    fn render_tone(stems: &[&str], bus: &str, blocks: usize) -> Vec<MixdownBlock> {
        let mut audio = headless();
        audio
            .begin_mixdown(&MixdownSettings {
                sample_rate: 48_000,
                stems: stems.iter().map(|s| s.to_string()).collect(),
            })
            .expect("mixdown");
        audio.play_asset(&tone(), on_bus(bus)).expect("play");
        audio.drain();
        (0..blocks)
            .map(|_| {
                audio.tick(1.0 / 60.0);
                audio.render_mixdown(800).expect("block")
            })
            .collect()
    }

    #[test]
    fn blocks_have_the_asked_length_for_the_mix_and_every_stem() {
        let blocks = render_tone(&["Music", "Sfx", "Reverb"], "Sfx", 3);
        for block in &blocks {
            assert_eq!(block.mix.len(), 1_600);
            assert_eq!(block.stems.len(), 3);
            assert!(block.stems.iter().all(|stem| stem.len() == 1_600));
        }
    }

    #[test]
    fn a_stem_hears_its_bus_and_not_the_others() {
        let blocks = render_tone(&["Music", "Sfx"], "Sfx", 4);
        let music: Vec<f32> = blocks.iter().flat_map(|b| b.stems[0].clone()).collect();
        let sfx: Vec<f32> = blocks.iter().flat_map(|b| b.stems[1].clone()).collect();
        let mix: Vec<f32> = blocks.iter().flat_map(|b| b.mix.clone()).collect();
        assert!(
            peak(&sfx) > 0.1,
            "the Sfx stem hears the tone, peak {}",
            peak(&sfx)
        );
        assert_eq!(peak(&music), 0.0, "the Music stem hears nothing");
        assert!(peak(&mix) > 0.1, "the mix has the tone");
    }

    #[test]
    fn stems_do_not_change_the_mix() {
        let plain = render_tone(&[], "Sfx", 4);
        let tapped = render_tone(&["Master", "Sfx", "World", "Reverb"], "Sfx", 4);
        for (a, b) in plain.iter().zip(&tapped) {
            let a: Vec<u32> = a.mix.iter().map(|s| s.to_bits()).collect();
            let b: Vec<u32> = b.mix.iter().map(|s| s.to_bits()).collect();
            assert_eq!(a, b, "the taps are silent in the mix");
        }
    }

    /// The Sfx stem of the tone played on Sfx after the bus was set to `db` and the fade settled.
    fn sfx_stem_at(db: f32) -> Vec<f32> {
        let mut audio = headless();
        audio
            .begin_mixdown(&MixdownSettings {
                sample_rate: 48_000,
                stems: vec!["Sfx".into()],
            })
            .expect("mixdown");
        audio
            .set_bus_volume("Sfx", db, std::time::Duration::ZERO)
            .expect("bus");
        // Let the bus reach its level before the tone starts.
        for _ in 0..6 {
            audio.tick(1.0 / 60.0);
            audio.render_mixdown(800).expect("settle");
        }
        audio.play_asset(&tone(), on_bus("Sfx")).expect("play");
        audio.drain();
        let mut stem = Vec::new();
        for _ in 0..4 {
            audio.tick(1.0 / 60.0);
            stem.extend(audio.render_mixdown(800).expect("block").stems.remove(0));
        }
        stem
    }

    #[test]
    fn a_stem_is_after_the_bus_fader() {
        let loud = peak(&sfx_stem_at(0.0));
        let quiet = peak(&sfx_stem_at(-6.0206));
        assert!(loud > 0.1, "the tone is heard, peak {loud}");
        let ratio = quiet / loud;
        assert!(
            (ratio - 0.5).abs() < 0.02,
            "post-fader stem: -6 dB halves it, ratio {ratio}"
        );
    }

    #[test]
    fn the_same_mixdown_twice_gives_the_same_samples() {
        let a = render_tone(&["Sfx"], "Sfx", 5);
        let b = render_tone(&["Sfx"], "Sfx", 5);
        let bits = |blocks: &[MixdownBlock]| -> Vec<u32> {
            blocks
                .iter()
                .flat_map(|b| b.mix.iter().chain(&b.stems[0]).map(|s| s.to_bits()))
                .collect()
        };
        assert_eq!(bits(&a), bits(&b));
    }

    #[test]
    fn begin_validates_and_changes_nothing_on_error() {
        let mut audio = headless();
        let bad_rate = MixdownSettings {
            sample_rate: 1_000,
            stems: vec![],
        };
        assert!(matches!(
            audio.begin_mixdown(&bad_rate),
            Err(AudioError::Invalid(_))
        ));
        let unknown = MixdownSettings {
            stems: vec!["Nope".into()],
            ..MixdownSettings::default()
        };
        assert_eq!(
            audio.begin_mixdown(&unknown),
            Err(AudioError::UnknownBus("Nope".into()))
        );
        let twice = MixdownSettings {
            stems: vec!["Sfx".into(), "Sfx".into()],
            ..MixdownSettings::default()
        };
        assert!(matches!(audio.begin_mixdown(&twice), Err(AudioError::Invalid(_))));
        assert!(!audio.mixdown_active());
        assert!(audio.render_mixdown(10).is_err(), "no mixdown runs");
        assert!(!audio.end_mixdown());
    }

    /// Deep-zai-audio-mix finding 4: the output lifecycle and a mixdown do not corrupt each
    /// other. A resume that arrives mid-mixdown defers to the mixdown's end (rebuilding the
    /// buses on the mixdown mixer would drop the stem taps), and ending a mixdown that was
    /// suspended leaves the output torn down. (A mixdown that STARTS on a suspended output is
    /// `a_mixdown_runs_while_the_output_is_suspended`.)
    #[test]
    fn mixdown_and_suspend_resume_stay_consistent() {
        let mut audio = headless();
        audio
            .begin_mixdown(&MixdownSettings::default())
            .expect("mixdown begins on a live output");
        // The device goes away mid-take (Android Suspended): the stream the mixdown stashed
        // is torn down too, and a resume that arrives before the take ends is deferred, not
        // applied to the mixdown mixer.
        audio.suspend_output();
        assert!(audio.output_suspended());
        assert!(
            !audio.resume_output(),
            "a resume mid-mixdown defers to the mixdown's end"
        );
        assert!(
            audio.output_suspended(),
            "deferring keeps the output suspended (the old code counted a resume that resumed nothing)"
        );
        assert!(audio.end_mixdown(), "the take ends");
        assert!(
            !audio.output_suspended(),
            "the deferred resume ran when the output came back"
        );
        // 1, not 2: the refused-take half this test opened with (and its resume) is gone; the
        // only resume is the deferred one.
        assert_eq!(
            audio.resume_count(),
            1,
            "the deferred resume is counted once, on the restored output"
        );
        assert!(!audio.mixdown_active());
        // The buses are live on the restored output: a play renders.
        audio.start_asset(&tone(), on_bus("World")).expect("plays");
        assert!(peak(&audio.render_offline(800).expect("render")) > 0.1);
    }

    /// Rev-kimi-28 (audio2-k item 2): a render-out take runs on its own offline mixer, so a
    /// suspended device output must not refuse it. The app suspends the output whenever every
    /// window is hidden and on a slow software adapter (lavapipe), and `AudioOut::start` turned
    /// the refusal into a failed `render.out` job. The device output stays suspended through
    /// the take and after it, until a resume (deferred when it arrives mid-take).
    #[test]
    fn a_mixdown_runs_while_the_output_is_suspended() {
        let mut audio = headless();
        audio.suspend_output();
        audio
            .begin_mixdown(&MixdownSettings::default())
            .expect("a take starts on a suspended output");
        assert!(audio.output_suspended(), "the device output stays suspended");
        audio
            .start_asset(&tone(), on_bus("World"))
            .expect("plays in the take");
        let block = audio.render_mixdown(800).expect("block");
        assert!(peak(&block.mix) > 0.1, "the take is not silent");
        assert!(audio.end_mixdown(), "the take ends");
        assert!(audio.output_suspended(), "no resume arrived: still torn down");
        assert_eq!(audio.resume_count(), 0);
        assert!(audio.resume_output(), "the next resume re-opens the output");
        audio.start_asset(&tone(), on_bus("World")).expect("plays");
        assert!(peak(&audio.render_offline(800).expect("render")) > 0.1);

        // Suspended before the take, resumed during it: the resume applies at the take's end.
        audio.suspend_output();
        audio
            .begin_mixdown(&MixdownSettings::default())
            .expect("a take starts on a suspended output");
        assert!(!audio.resume_output(), "a resume mid-take defers");
        assert!(audio.end_mixdown());
        assert!(!audio.output_suspended(), "the deferred resume ran");
        assert_eq!(audio.resume_count(), 2);
        audio.start_asset(&tone(), on_bus("World")).expect("plays");
        assert!(peak(&audio.render_offline(800).expect("render")) > 0.1);
    }

    /// Renderout audit 3: the frame a job starts on has already measured a wall-clock delta, and
    /// `audio_sync` hands it to `tick`. A 16 ms frame, a hitch and a paused clock must all start
    /// the job's mix from the same clocks.
    #[test]
    fn the_arming_frames_wall_clock_delta_never_reaches_the_mix_clock() {
        for arming_delta in [0.016, 0.25, 0.0] {
            let mut audio = headless();
            audio.tick(0.5);
            let before = audio.now_secs;
            audio.begin_mixdown(&MixdownSettings::default()).expect("begin");
            audio.tick(arming_delta);
            assert_eq!(audio.now_secs, before, "arming delta {arming_delta}");
            audio.tick(0.1);
            assert_eq!(audio.now_secs, before + 0.1, "the job's own steps count");
        }
    }

    /// The mixdown's offline mixer meters on its own: what it rendered does not land in the
    /// output's meter (and the output's blocks never land in the mixdown's), so the output meter
    /// after a mixdown describes the output only (concurrency audit 11-03 L1).
    #[test]
    fn a_mixdown_does_not_count_into_the_output_meter() {
        let mut audio = headless();
        audio.reset_output_meter();
        audio
            .begin_mixdown(&MixdownSettings {
                sample_rate: 48_000,
                stems: Vec::new(),
            })
            .expect("mixdown");
        for _ in 0..3 {
            audio.tick(1.0 / 60.0);
            audio.render_mixdown(800).expect("block");
        }
        assert_eq!(
            audio.output_meter().frames,
            2_400,
            "while it runs the meter shows the mixdown"
        );
        assert!(audio.end_mixdown());
        assert_eq!(
            audio.output_meter().frames,
            0,
            "the mixdown's frames were counted as output"
        );
    }

    #[test]
    fn one_mixdown_at_a_time_and_end_restores_the_output() {
        let mut audio = headless();
        let settings = MixdownSettings {
            sample_rate: 44_100,
            stems: vec!["Music".into()],
        };
        audio.begin_mixdown(&settings).expect("first");
        assert_eq!(audio.mixdown(), Some(&settings));
        assert!(audio.begin_mixdown(&settings).is_err(), "a second one is refused");
        assert!(audio.end_mixdown());
        assert!(!audio.mixdown_active());
        assert!(audio.render_mixdown(1).is_err());
        // The headless output is back and still renders, with its buses rebuilt.
        audio
            .play_asset(&tone(), on_bus("Sfx"))
            .expect("play after the mixdown");
        audio.drain();
        assert!(peak(&audio.render_offline(2_400).expect("render")) > 0.1);
    }
}
