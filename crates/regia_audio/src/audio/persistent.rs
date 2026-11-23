// SPDX-License-Identifier: MPL-2.0
//! Persistent output recovery (ADR-8052). Ownership survives; mixer handles do not.

use super::{Audio, resolved_params_bus, stop_patch_loops};
use crate::{
    AudioError, LoopRegion, PlaybackParams, SoundAsset, SoundInstanceId, VoiceEntry, VoiceKind,
    voice_priority,
};

impl Audio {
    /// Starts a host-owned 2D loop that recovers after output suspension, keeping its voice id.
    ///
    /// Uses `params.loop_region`, then the asset's default region, then the whole sound. The
    /// playback cursor freezes while suspended. Pause, volume, pitch, pan and seek controls
    /// survive recovery; [`Self::stop_voice`] cancels recovery even while suspended. A call
    /// while the mixer is down (suspended, and no render-out take is running) registers the
    /// loop without opening a mixer voice until output resumes. A call during a render-out
    /// take plays in that take and ends with the take. Recovery uses a short fade-in and
    /// respects the normal voice budget. Beginning or ending a mixdown ends a loop that is
    /// already playing, like other preview audio. This does not reconnect live voice-chat feeds.
    ///
    /// # Errors
    /// Returns [`AudioError::EmptyClip`] for an empty sound, [`AudioError::UnknownBus`] for an
    /// unknown bus, [`AudioError::Invalid`] for an unknown/non-finite duration, or a decoder/
    /// backend error when the sound cannot start. A deferred start reports decoder failures
    /// through the audio log when output resumes.
    pub fn start_persistent_loop(
        &mut self,
        asset: &SoundAsset,
        mut params: PlaybackParams,
    ) -> Result<SoundInstanceId, AudioError> {
        if !asset.duration_secs().is_finite() {
            return Err(AudioError::Invalid(
                "a persistent loop needs a finite duration".into(),
            ));
        }
        if asset.duration_secs() <= 0.0 {
            return Err(AudioError::EmptyClip);
        }
        params.loop_region = Some(
            params
                .loop_region
                .or(asset.default_loop_region())
                .unwrap_or(LoopRegion {
                    start_secs: 0.0,
                    end_secs: None,
                }),
        );
        let id = if self.mixer_down() {
            let bus = resolved_params_bus(&mut params, Some(asset));
            if !self.buses.contains(&bus) {
                return Err(AudioError::UnknownBus(bus));
            }
            let id = self.voices.push(VoiceEntry {
                id: SoundInstanceId(0),
                kind: VoiceKind::OneShot,
                bus: bus.into(),
                cue: None,
                asset: asset.clone(),
                base_volume_db: params.volume_db,
                virtual_cursor_secs: params.start_offset_secs,
                params,
                gain: 1.0,
                priority: voice_priority::NORMAL,
                instance: None,
                filter_track: None,
                age_secs: 0.0,
                stopping: false,
                paused: false,
                held: false,
            });
            self.played_count += 1;
            id
        } else {
            self.start_asset(asset, params)?
        };
        self.persistent_loops.insert(id);
        Ok(id)
    }

    /// Number of host-owned loops eligible for output recovery (real, virtual or suspended).
    /// Stopped loops are excluded immediately. Parametric patches are counted separately by
    /// [`Self::persistent_patch_count`].
    pub fn persistent_loop_count(&self) -> usize {
        self.persistent_loops
            .iter()
            .filter(|id| self.voices.get(**id).is_some_and(|entry| !entry.stopping))
            .count()
    }

    /// Number of spawned parametric patches, including patches waiting for output recovery.
    pub fn persistent_patch_count(&self) -> usize {
        self.patches.len()
    }

    /// Drops transient voices and all old mixer handles, retaining only restartable ownership.
    pub(super) fn suspend_persistent_voices(&mut self) {
        self.stop_voice_loopback();
        self.music.output_lost();
        for entry in &mut self.voices.entries {
            entry.virtualise();
        }
        self.voices.retain_for_output_resume(&self.persistent_loops);
        self.persistent_loops.retain(|id| self.voices.get(*id).is_some());
        for player in &mut self.patches {
            stop_patch_loops(&mut [], &mut player.layers);
            player.layers.clear();
        }
    }

    /// Restarts only on the rebuilt bus tree, using retained ids, controls and parameters.
    #[tracing::instrument(target = "regia_audio", skip_all, level = "trace")]
    pub(super) fn restart_persistent_voices(&mut self) {
        // Reconcile in voice order, not HashSet order, and obey the budget on the first frame.
        for id in self.voices.reconcile().revive {
            self.revive_voice(id);
        }
        let mut patches = std::mem::take(&mut self.patches);
        for player in &mut patches {
            match self.build_patch_layers(&player.patch, &player.params) {
                Ok(layers) => player.layers = layers,
                Err(error) => {
                    tracing::warn!(target: "regia_audio", patch = player.id, "patch recovery failed: {error}")
                }
            }
        }
        self.patches = patches;
    }

    /// Records the destination of a host volume tween, not a dead mixer's in-flight tween.
    pub(super) fn retain_persistent_volume(&mut self, id: SoundInstanceId, mut db: f32) -> f32 {
        if self.persistent_loops.contains(&id)
            && let Some(entry) = self.voices.get_mut(id)
        {
            db = crate::sound::safe_volume_db(db);
            // Setters target the absolute level; remove asset offsets from the retained copy
            // rather than subtracting them (which can overflow for extreme authored values).
            entry.asset = entry
                .asset
                .clone()
                .with_default_volume_db(0.0)
                .with_loudness_gain_db(0.0);
            entry.params.volume_db = db;
            entry.base_volume_db = db;
        }
        db
    }

    pub(super) fn retain_persistent_pitch(&mut self, id: SoundInstanceId, mut ratio: f32) -> f32 {
        if self.persistent_loops.contains(&id)
            && let Some(entry) = self.voices.get_mut(id)
        {
            // The handle setter is absolute. Normalize the retained asset's pitch so even an
            // asset authored at zero pitch can subsequently restart at a host-selected rate.
            ratio = crate::sound::safe_playback_rate(ratio) as f32;
            entry.asset = entry.asset.clone().with_default_pitch(1.0);
            entry.params.pitch = ratio;
        }
        ratio
    }

    pub(super) fn retain_persistent_pan(&mut self, id: SoundInstanceId, mut pan: f32) -> f32 {
        if self.persistent_loops.contains(&id)
            && let Some(entry) = self.voices.get_mut(id)
        {
            pan = crate::sound::safe_pan(pan);
            entry.params.panning = Some(pan);
        }
        pan
    }

    pub(super) fn retain_persistent_seek(&mut self, id: SoundInstanceId, mut secs: f64) -> f64 {
        if self.persistent_loops.contains(&id)
            && let Some(entry) = self.voices.get_mut(id)
        {
            if !secs.is_finite() {
                crate::sound::note_rejected("persistent loop seek");
                secs = 0.0;
            }
            secs = secs.max(0.0);
            entry.virtual_cursor_secs = secs;
        }
        secs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AudioSettings, SoundClip};

    #[test]
    fn failed_bus_rebuild_retains_ownership_until_a_successful_retry() {
        let mut audio = Audio::new(&AudioSettings {
            headless: true,
            ..AudioSettings::default()
        });
        let asset = SoundAsset::from_clip(&SoundClip::new(vec![0.1; 4_800], 48_000).expect("clip"));
        let id = audio
            .start_persistent_loop(&asset, PlaybackParams::default())
            .expect("loop");
        audio.suspend_output();
        let valid = audio.bus_layout.clone();
        // Force a deterministic rebuild refusal, without needing a real device failure.
        audio.bus_layout.buses[0].parent = Some("missing_parent".into());
        assert!(!audio.resume_output());
        assert!(audio.output_suspended());
        assert_eq!(audio.resume_count(), 0);
        assert_eq!(audio.persistent_loop_count(), 1);
        assert_eq!(audio.real_voice_count(), 0);
        assert!(audio.voice_alive(id));
        audio.bus_layout = valid;
        assert!(audio.resume_output());
        assert_eq!(audio.resume_count(), 1);
        assert_eq!(audio.real_voice_count(), 1);
        assert!(audio.voice_alive(id));
    }
}
