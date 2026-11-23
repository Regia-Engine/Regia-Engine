// SPDX-License-Identifier: MPL-2.0
//! `.ambience.ron` ambiences on the audio system (ADR-2721): each playing file's bed voices and
//! its seeded one-shot calls.

use std::time::Duration;

use regia_core::math::Vec3;

use super::Audio;
use crate::bus::MUTE_DB;
use crate::{
    AmbienceDef, AmbienceError, AmbienceInputs, AmbiencePlayer, AudioEmitter, LoopRegion, PlaybackParams,
    SoundAsset, SoundInstanceId,
};

/// Fade of a stopped ambience's bed voices.
const AMBIENCE_STOP_FADE: Duration = Duration::from_millis(500);

/// The playing ambience files of an [`Audio`].
#[derive(Default)]
pub(crate) struct AmbienceFiles {
    playing: Vec<PlayingAmbience>,
}

struct PlayingAmbience {
    player: AmbiencePlayer,
    /// Per bed layer: its looping voice and the sound it plays.
    voices: Vec<Option<(SoundInstanceId, String)>>,
    /// One-shot calls whose sound was not registered.
    missing: u64,
    played: u64,
}

/// A playing ambience (`audio.ambience_list`).
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct AmbienceFileInfo {
    /// The file's name.
    pub name: String,
    /// Its seed.
    pub seed: u64,
    /// The ambience clock, seconds.
    pub clock_secs: f64,
    /// `(bed layer, level dB now)`.
    pub beds: Vec<(String, f32)>,
    /// One-shot names.
    pub one_shots: Vec<String>,
    /// One-shot calls played.
    pub calls_played: u64,
    /// Calls silenced by their curve.
    pub calls_skipped: u64,
    /// Calls whose sound is not registered.
    pub calls_missing: u64,
}

impl Audio {
    /// Starts the ambience `def` (replacing a playing one of the same name, whose beds fade out).
    /// Its beds and one-shots play from the next [`Audio::step_ambience_files`].
    ///
    /// # Errors
    /// The file's validation error.
    pub fn play_ambience(&mut self, def: AmbienceDef) -> Result<(), AmbienceError> {
        let player = AmbiencePlayer::new(def)?;
        let name = player.def().name.clone();
        self.stop_ambience(&name);
        self.ambience_files.playing.push(PlayingAmbience {
            voices: vec![None; player.def().beds.len()],
            player,
            missing: 0,
            played: 0,
        });
        Ok(())
    }

    /// Stops the ambience `name`: its bed voices fade out, calls already playing finish. False
    /// when none plays.
    pub fn stop_ambience(&mut self, name: &str) -> bool {
        let Some(at) = self
            .ambience_files
            .playing
            .iter()
            .position(|p| p.player.def().name == name)
        else {
            return false;
        };
        let stopped = self.ambience_files.playing.remove(at);
        for (id, _) in stopped.voices.into_iter().flatten() {
            self.stop_voice(id, AMBIENCE_STOP_FADE);
        }
        true
    }

    /// The playing ambiences, in start order.
    pub fn ambience_files(&self) -> Vec<AmbienceFileInfo> {
        self.ambience_files
            .playing
            .iter()
            .map(|p| {
                let def = p.player.def();
                let bed = p.player.bed();
                let (_, skipped) = p.player.call_counts();
                AmbienceFileInfo {
                    name: def.name.clone(),
                    seed: def.seed,
                    clock_secs: p.player.clock_secs(),
                    beds: def
                        .beds
                        .iter()
                        .enumerate()
                        .map(|(i, layer)| (layer.name.clone(), bed.voice_db(i)))
                        .collect(),
                    one_shots: def.one_shots.iter().map(|s| s.name.clone()).collect(),
                    calls_played: p.played,
                    calls_skipped: skipped,
                    calls_missing: p.missing,
                }
            })
            .collect()
    }

    /// Advances every playing ambience by `dt` with the scene `inputs` and the listener at
    /// `listener`: starts each bed layer's loop once `sound` resolves its name, sets the layer
    /// levels, and plays the one-shot calls that fell due at their places. Returns the calls
    /// played. The `AudioPlugin` system calls it every frame; tools and tests call it directly.
    pub fn step_ambience_files(
        &mut self,
        dt: f32,
        inputs: &AmbienceInputs,
        listener: Vec3,
        sound: &dyn Fn(&str) -> Option<SoundAsset>,
    ) -> usize {
        if self.ambience_files.playing.is_empty() {
            return 0;
        }
        let _span =
            tracing::trace_span!("ambience_files", playing = self.ambience_files.playing.len()).entered();
        let mut playing = std::mem::take(&mut self.ambience_files.playing);
        let mut played = 0;
        for amb in &mut playing {
            let calls = amb.player.step(inputs, dt, listener);
            let bus = amb.player.def().bus.clone();
            for i in 0..amb.voices.len() {
                let wanted = amb.player.def().beds[i].sound.clone();
                if let Some((id, _)) = &amb.voices[i]
                    && !self.voice_alive(*id)
                {
                    amb.voices[i] = None;
                }
                if amb.voices[i].as_ref().map(|(_, s)| s) != wanted.as_ref() {
                    if let Some((id, _)) = amb.voices[i].take() {
                        self.stop_voice(id, AMBIENCE_STOP_FADE);
                    }
                    if let Some(name) = wanted
                        && let Some(asset) = sound(&name)
                    {
                        let params = PlaybackParams {
                            bus: Some(bus.clone()),
                            volume_db: MUTE_DB,
                            loop_region: Some(LoopRegion {
                                start_secs: 0.0,
                                end_secs: None,
                            }),
                            ..PlaybackParams::default()
                        };
                        match self.start_asset(&asset, params) {
                            Ok(id) => amb.voices[i] = Some((id, name)),
                            Err(error) => {
                                tracing::warn!(target: "regia_audio", "ambience bed `{name}` not started: {error}");
                            }
                        }
                    }
                }
                if let Some((id, _)) = &amb.voices[i] {
                    let db = amb.player.bed().voice_db(i);
                    self.set_voice_volume_db(*id, db, dt.clamp(1.0 / 240.0, 0.1));
                }
            }
            for call in calls {
                let Some(asset) = sound(&call.sound) else {
                    amb.missing += 1;
                    continue;
                };
                let params = PlaybackParams {
                    bus: Some(bus.clone()),
                    volume_db: call.volume_db,
                    ..PlaybackParams::default()
                };
                let emitter = AudioEmitter {
                    max_distance: (call.position - listener).length().max(1.0) * 4.0,
                    ..AudioEmitter::default()
                };
                match self.start_asset_at(&asset, params, call.position, &emitter) {
                    Ok(_) => {
                        amb.played += 1;
                        played += 1;
                    }
                    Err(error) => tracing::debug!(target: "regia_audio", "ambience call not played: {error}"),
                }
            }
        }
        self.ambience_files.playing = playing;
        played
    }
}
