// SPDX-License-Identifier: MPL-2.0
//! Mixer inspection and the master output stage on the [`Audio`] facade (ADR-544): the state
//! of every bus, the master limiter and the output meter, for tools, tests and the agent
//! protocol.

use super::Audio;
use crate::bus::MUTE_DB;
use crate::error::AudioError;
use crate::limiter::{MasterLimiter, OutputMeter};
use crate::render_stats::RenderStats;

/// One bus's mixer state (see [`Audio::bus_status`]).
#[derive(Clone, Debug, PartialEq)]
pub struct BusStatus {
    /// Bus name.
    pub name: String,
    /// Parent bus (`None` for the root).
    pub parent: Option<String>,
    /// The layout's volume, dB.
    pub layout_db: f32,
    /// The host's volume (sliders, [`Audio::set_bus_volume`]), dB: the snapshot-independent
    /// base.
    pub base_db: f32,
    /// Where the snapshot blend is heading, dB.
    pub target_db: f32,
    /// Where the snapshot blend is now, dB.
    pub current_db: f32,
    /// The last volume sent to the mixer (blend, ducking and mute composed), dB; `None` before
    /// the first tick.
    pub applied_db: Option<f32>,
    /// Muted ([`Audio::set_bus_mute`], user settings, or the layout's `mute`).
    pub muted: bool,
    /// Voices on the bus bypass the propagation medium.
    pub bypass_medium: bool,
    /// Sends into send buses: `(send bus, layout level dB)`.
    pub sends: Vec<(String, f32)>,
}

impl BusStatus {
    /// The bus's level relative to the layout once applied (ducking included), dB; the mute
    /// floor while muted.
    pub fn applied_offset_db(&self) -> Option<f32> {
        self.applied_db
            .map(|db| if self.muted { MUTE_DB } else { db - self.layout_db })
    }
}

impl Audio {
    /// Every bus of the layout, in layout order (parents first), with its blend, the last level
    /// applied and its mute.
    pub fn bus_status(&self) -> Vec<BusStatus> {
        self.bus_layout
            .buses
            .iter()
            .filter_map(|def| {
                let blend = self.bus_mix.get(&def.name)?;
                Some(BusStatus {
                    name: def.name.clone(),
                    parent: def.parent.clone(),
                    layout_db: def.volume_db,
                    base_db: self.user_base_db.get(&def.name).copied().unwrap_or(def.volume_db),
                    target_db: blend.target_db,
                    current_db: blend.current_db,
                    applied_db: (!blend.last_applied_db.is_nan()).then_some(blend.last_applied_db),
                    muted: blend.muted,
                    bypass_medium: def.bypass_medium,
                    sends: def.sends.iter().map(|s| (s.bus.clone(), s.volume_db)).collect(),
                })
            })
            .collect()
    }

    /// The levels of every regular bus, in layout order, post-fader (`bus_meter.rs`, ADR-2530).
    /// Lock-free: it reads atomics the mixer thread publishes.
    pub fn bus_meters(&self) -> Vec<(String, crate::bus_meter::BusMeterReading)> {
        self.bus_layout
            .buses
            .iter()
            .filter_map(|def| Some((def.name.clone(), self.buses.meter(&def.name)?.read())))
            .collect()
    }

    /// The master limiter in effect on the output (`None` = bypassed). On by default
    /// ([`MasterLimiter::default`]: 0 dBFS ceiling, 80 ms release).
    pub fn master_limiter(&self) -> Option<MasterLimiter> {
        self.backend.limiter().get()
    }

    /// Sets the master limiter (clamped into its ranges), or bypasses it with `None`. Takes
    /// effect at the next audio block. Bypassed, kira's hard clip at full scale is what limits
    /// the output; NaN and infinite samples are silenced either way.
    pub fn set_master_limiter(&mut self, limiter: Option<MasterLimiter>) {
        self.backend.set_limiter(limiter);
    }

    /// What the output stage measured since the last [`Audio::reset_output_meter`]: frames,
    /// peaks before and after the limiter, gain reduction, non-finite samples.
    pub fn output_meter(&self) -> OutputMeter {
        self.backend.limiter().meter()
    }

    /// Downmixes the whole output to mono (ADR-2646, a player accessibility choice): both channels
    /// carry `(left + right) / 2`, after every bus and before the master limiter, so a sound panned
    /// hard to one side is still heard in both ears. Off by default; takes effect at the next audio
    /// block and survives an output rebuild.
    pub fn set_mono_output(&mut self, mono: bool) {
        self.backend.limiter().set_mono(mono);
    }

    /// Whether the output is downmixed to mono ([`Audio::set_mono_output`]).
    pub fn mono_output(&self) -> bool {
        self.backend.limiter().mono()
    }

    /// Starts a new output-meter window.
    pub fn reset_output_meter(&mut self) {
        self.backend.limiter().reset_meter();
    }

    /// The live program-loudness meter of the output (ADR-7227, `docs/specs/audio-loudness.md`
    /// § 6): momentary, short-term and gated integrated loudness, loudness range and true peak of
    /// everything the master limiter lets out, since the meter was turned on or reset. Updated every
    /// 100 ms of output; lock-free. Off by default ([`Audio::set_program_loudness_enabled`]).
    pub fn program_loudness(&self) -> crate::ProgramLoudness {
        self.backend.limiter().program().read()
    }

    /// Turns the program-loudness meter on or off. Turning it on starts a new measurement at the
    /// next audio block; while off it costs nothing. It stays on across an output rebuild.
    pub fn set_program_loudness_enabled(&mut self, enabled: bool) {
        self.backend.limiter().program().set_enabled(enabled);
    }

    /// Starts a new program-loudness measurement (integrated loudness, range and true peak from now).
    pub fn reset_program_loudness(&mut self) {
        self.backend.limiter().program().reset();
    }

    /// Renders `frames` stereo frames on the offline backend (like [`Audio::render_offline`])
    /// and measures them. `Err(NeedsOffline)` on a device.
    pub fn render_offline_measured(&mut self, frames: usize) -> Result<(Vec<f32>, RenderStats), AudioError> {
        let _span = tracing::info_span!("audio_render_measured", frames).entered();
        let out = self.render_offline(frames)?;
        let stats = RenderStats::measure(&out);
        Ok((out, stats))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::AudioSettings;

    fn headless() -> Audio {
        Audio::new(&AudioSettings {
            headless: true,
            ..AudioSettings::default()
        })
    }

    #[test]
    fn mono_output_is_off_by_default_and_round_trips() {
        let mut audio = headless();
        assert!(!audio.mono_output());
        audio.set_mono_output(true);
        assert!(audio.mono_output());
        audio.set_mono_output(false);
        assert!(!audio.mono_output());
    }

    #[test]
    fn bus_status_lists_the_layout_in_order_with_its_mix() {
        let mut audio = headless();
        audio
            .set_bus_volume("Music", -12.0, std::time::Duration::ZERO)
            .expect("Music");
        audio.set_bus_mute("Ui", true).expect("Ui");
        let before = audio.bus_status();
        assert_eq!(before[0].name, "Master");
        assert_eq!(
            before[0].applied_db, None,
            "nothing applied before the first tick"
        );
        audio.tick(1.0 / 60.0);
        let status = audio.bus_status();
        let names: Vec<&str> = status.iter().map(|b| b.name.as_str()).collect();
        assert_eq!(
            names,
            ["Master", "Music", "Sfx", "World", "Ui", "Voice", "Ambience"]
        );
        let music = &status[1];
        assert_eq!(
            (music.layout_db, music.base_db, music.target_db),
            (-3.0, -12.0, -12.0)
        );
        assert_eq!(music.applied_db, Some(-12.0));
        assert_eq!(music.applied_offset_db(), Some(-9.0));
        assert!(music.bypass_medium);
        let ui = &status[4];
        assert!(ui.muted);
        assert_eq!(ui.applied_db, Some(MUTE_DB));
        assert_eq!(ui.applied_offset_db(), Some(MUTE_DB));
        assert_eq!(
            status[3].sends,
            vec![("Reverb".to_string(), crate::bus::SILENT_SEND_DB)]
        );
    }

    #[test]
    fn the_limiter_is_on_by_default_and_can_be_bypassed() {
        let mut audio = headless();
        assert_eq!(audio.master_limiter(), Some(MasterLimiter::default()));
        audio.set_master_limiter(None);
        assert_eq!(audio.master_limiter(), None);
        audio.set_master_limiter(Some(MasterLimiter {
            ceiling_db: -3.0,
            release_secs: 0.2,
        }));
        let limiter = audio.master_limiter().expect("on");
        assert!((limiter.ceiling_db + 3.0).abs() < 1e-4);
        let (out, stats) = audio.render_offline_measured(256).expect("offline");
        assert_eq!(out.len(), 512);
        assert_eq!(stats.frames, 256);
        assert_eq!(audio.output_meter().frames, 256, "the meter saw the blocks");
        audio.reset_output_meter();
        assert_eq!(audio.output_meter(), OutputMeter::default());
    }

    #[test]
    fn bus_meters_follow_a_played_tone_and_a_mute() {
        use crate::clip::SoundClip;
        let mut audio = headless();
        let clip = SoundClip::new(
            (0..48_000)
                .map(|i| ((i as f32 / 48_000.0) * 1_000.0 * std::f32::consts::TAU).sin() * 0.5)
                .collect::<Vec<_>>(),
            48_000,
        )
        .expect("tone");
        audio.play(&clip).expect("enqueue");
        audio.drain();
        audio.render_offline(9_600).expect("render");
        let meters = audio.bus_meters();
        let names: Vec<&str> = meters.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"Master") && names.contains(&"Music"), "{names:?}");
        let level = |audio: &Audio, bus: &str| {
            audio
                .bus_meters()
                .into_iter()
                .find(|(n, _)| n == bus)
                .map(|(_, r)| r)
                .expect("bus has a meter")
        };
        let master = level(&audio, "Master");
        assert!(
            master.peak_db > -12.0 && master.peak_db < 0.5,
            "the tone is audible on Master, peak {}",
            master.peak_db
        );
        assert_eq!(
            level(&audio, "Music").peak_db,
            f32::NEG_INFINITY,
            "nothing plays on Music"
        );
    }
}
