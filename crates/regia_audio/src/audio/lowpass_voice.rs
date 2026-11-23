// SPDX-License-Identifier: MPL-2.0
//! Low-pass voices (ADR-3197): a voice on its own child track under its bus, with a low-pass
//! filter whose cutoff the host moves while it plays. The sequencer plays occluded spatial sound
//! sections through it (the emitter tracks' occlusion filter, for a voice that has no emitter
//! entity). A voice without a low-pass keeps the plain path ([`Audio::start_asset`]).

use std::collections::HashMap;

use kira::{
    effect::filter::{FilterBuilder, FilterHandle, FilterMode},
    track::TrackBuilder,
};

use super::{Audio, play_data_on_track, resolved_params_bus};
use crate::bus::tween;
use crate::error::AudioError;
use crate::sound::{PlaybackParams, SoundAsset, tween_from_secs};
use crate::voices::{SoundInstanceId, VoiceKind};

/// The filters of the playing low-pass voices.
#[derive(Default)]
pub(crate) struct LowpassVoices {
    filters: HashMap<SoundInstanceId, FilterHandle>,
}

/// A cutoff kira can use: finite, 20 Hz to 20 kHz (a NaN would poison the filter state).
fn safe_cutoff(cutoff_hz: f32) -> f64 {
    if cutoff_hz.is_finite() {
        f64::from(cutoff_hz.clamp(20.0, 20_000.0))
    } else {
        crate::sound::note_rejected("low-pass cutoff");
        20_000.0
    }
}

impl Audio {
    /// [`Audio::start_asset`] on a one-off child track of the voice's bus with a low-pass at
    /// `cutoff_hz` (20 Hz to 20 kHz; a non-finite cutoff is refused and opens the filter) that
    /// [`Audio::set_voice_lowpass`] moves while it plays. The voice is a normal voice otherwise:
    /// [`Audio::set_voice_volume_db`], [`Audio::set_voice_pitch`], [`Audio::set_voice_pan`] and
    /// [`Audio::stop_voice`] work on it, and bus volume, ducking and snapshots apply. When the
    /// bus cannot host a child track the voice plays unfiltered on the bus (logged).
    ///
    /// # Errors
    /// [`AudioError::UnknownBus`]; the errors of [`Audio::start_asset`].
    pub fn start_asset_lowpass(
        &mut self,
        asset: &SoundAsset,
        mut params: PlaybackParams,
        cutoff_hz: f32,
    ) -> Result<SoundInstanceId, AudioError> {
        let voices = &self.voices;
        self.lowpass.filters.retain(|id, _| voices.get(*id).is_some());
        // The params' bus, else the asset's default bus, recorded in `params.bus` as `start_asset`
        // does, so the voice's bookkeeping names the track it plays on.
        let bus = resolved_params_bus(&mut params, Some(asset));
        let mut builder = TrackBuilder::new().persist_until_sounds_finish(true);
        let filter = builder.add_effect(
            FilterBuilder::new()
                .mode(FilterMode::LowPass)
                .cutoff(safe_cutoff(cutoff_hz))
                .resonance(0.0),
        );
        let Some(mut track) = self.buses.add_child_track(&bus, builder) else {
            if !self.buses.contains(&bus) {
                return Err(AudioError::UnknownBus(bus));
            }
            tracing::warn!(target: "regia_audio", "low-pass track unavailable on `{bus}`, playing unfiltered");
            return self.start_asset(asset, params);
        };
        let instance = play_data_on_track(&mut track, asset.play_data(&params)?)?;
        let id = self.register_voice(
            asset.clone(),
            params,
            VoiceKind::OneShot,
            None,
            1.0,
            instance,
            None,
        );
        if let Some(entry) = self.voices.get_mut(id) {
            entry.filter_track = Some(track);
        }
        self.lowpass.filters.insert(id, filter);
        self.played_count += 1;
        Ok(id)
    }

    /// Moves a low-pass voice's cutoff to `cutoff_hz` over `tween_secs`. `false` (nothing
    /// changes) for a voice that is gone or has no low-pass.
    pub fn set_voice_lowpass(&mut self, id: SoundInstanceId, cutoff_hz: f32, tween_secs: f32) -> bool {
        if self.voices.get(id).is_none() {
            self.lowpass.filters.remove(&id);
            return false;
        }
        match self.lowpass.filters.get_mut(&id) {
            Some(filter) => {
                filter.set_cutoff(safe_cutoff(cutoff_hz), tween(tween_from_secs(tween_secs)));
                true
            }
            None => false,
        }
    }

    /// `true` while `id` is a live voice started by [`Audio::start_asset_lowpass`] with its
    /// filter track.
    #[must_use]
    pub fn voice_has_lowpass(&self, id: SoundInstanceId) -> bool {
        self.voices.get(id).is_some() && self.lowpass.filters.contains_key(&id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AudioSettings, MixdownSettings, SoundClip};

    /// A 6 kHz tone, 1 s at 48 kHz.
    fn bright() -> SoundAsset {
        let samples: Vec<f32> = (0..48_000)
            .map(|i| (std::f32::consts::TAU * 6_000.0 * i as f32 / 48_000.0).sin() * 0.5)
            .collect();
        SoundAsset::from_clip(&SoundClip::new(samples, 48_000).expect("clip"))
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|s| s * s).sum::<f32>() / x.len().max(1) as f32).sqrt()
    }

    /// Renders `blocks` of 800 frames of one voice, with the cutoff moved to `closed_hz` after the
    /// first four blocks (`None`: a plain voice). Returns (rms before, rms after).
    fn render(closed_hz: Option<f32>) -> (f32, f32) {
        let mut audio = Audio::new(&AudioSettings {
            headless: true,
            ..AudioSettings::default()
        });
        audio.begin_mixdown(&MixdownSettings::default()).expect("begin");
        let params = PlaybackParams {
            bus: Some("Sfx".into()),
            ..PlaybackParams::default()
        };
        let id = match closed_hz {
            Some(_) => audio
                .start_asset_lowpass(&bright(), params, 20_000.0)
                .expect("start"),
            None => audio.start_asset(&bright(), params).expect("start"),
        };
        assert_eq!(audio.voice_has_lowpass(id), closed_hz.is_some());
        let mut before = Vec::new();
        for _ in 0..4 {
            audio.tick(1.0 / 60.0);
            before.extend(audio.render_mixdown(800).expect("block").mix);
        }
        if let Some(hz) = closed_hz {
            assert!(audio.set_voice_lowpass(id, hz, 0.0));
        }
        let mut after = Vec::new();
        for _ in 0..8 {
            audio.tick(1.0 / 60.0);
            after.extend(audio.render_mixdown(800).expect("block").mix);
        }
        // Skip the first two blocks after the change (the filter settles).
        (rms(&before[1600..]), rms(&after[3200..]))
    }

    /// An open low-pass voice is heard like a plain one; closing it to 500 Hz takes a 6 kHz tone
    /// down by more than 30 dB; an unknown voice reports `false`.
    #[test]
    fn a_lowpass_voice_follows_its_cutoff() {
        let (plain, _) = render(None);
        let (open, closed) = render(Some(500.0));
        assert!(plain > 0.05, "the tone is heard: {plain}");
        assert!(
            (open / plain - 1.0).abs() < 0.05,
            "open filter {open} vs plain {plain}"
        );
        assert!(closed < open * 0.03, "closed {closed} vs open {open}");
        let mut audio = Audio::new(&AudioSettings {
            headless: true,
            ..AudioSettings::default()
        });
        assert!(!audio.set_voice_lowpass(SoundInstanceId(99), 500.0, 0.0));
    }
}
