// SPDX-License-Identifier: MPL-2.0
//! One-shot and granular patch layers and the patch state for tools (`.audiopatch.ron` v2,
//! ADR-7226, `docs/specs/16-audio.md` § 3a).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use kira::track::TrackHandle;

use super::{Audio, PatchLayerRuntime, play_data_on, play_data_on_track};
use crate::bus::{Buses, finite_db, tween};
use crate::error::AudioError;
use crate::granular::{GranularControls, GranularHandle, GranularSoundData};
use crate::patch::{
    LayerOutputs, PatchGranularDef, PatchLayerDef, PatchOneShotDef, SmoothedParam, granular_controls,
};
use crate::sound::{PlaybackParams, SoundAsset};
use crate::spatial::db_to_gain;

/// The trigger state of a one-shot layer.
pub(crate) struct OneShotRuntime {
    /// Index of the sound the next shot plays (round-robin).
    next: usize,
    /// Clock time before which a shot is skipped (cooldown).
    cooldown_until: f64,
    /// Shots played.
    fired: u64,
    /// Shots skipped (cooldown, or the mixer refused the sound).
    skipped: u64,
    /// The watched parameter's smoothed value at the last tick.
    last: f32,
}

impl OneShotRuntime {
    /// A runtime that remembers the watched parameter's starting value, so the spawn itself
    /// never counts as a crossing.
    pub(crate) fn new(def: &PatchOneShotDef, params: &HashMap<String, SmoothedParam>) -> Self {
        let last = def
            .trigger
            .parameter()
            .and_then(|name| params.get(name))
            .map_or(0.0, |p| p.value);
        Self {
            next: 0,
            cooldown_until: f64::NEG_INFINITY,
            fired: 0,
            skipped: 0,
            last,
        }
    }
}

/// The pieces of `Audio` and of the patch a shot needs (borrowed apart by the tick).
pub(super) struct ShotContext<'a> {
    /// The mixer buses.
    pub(super) buses: &'a mut Buses,
    /// The patch's bus.
    pub(super) bus: &'a str,
    /// The patch's resolved `(layer, index)` sounds.
    pub(super) sounds: &'a HashMap<(String, usize), SoundAsset>,
    /// The patch clock, seconds.
    pub(super) clock: f64,
}

/// Plays the layer's next one-shot at the layer's current outputs; returns whether it started.
fn fire(
    ctx: &mut ShotContext<'_>,
    layer_def: &PatchLayerDef,
    def: &PatchOneShotDef,
    layer: &mut PatchLayerRuntime,
    out: &LayerOutputs,
) -> bool {
    let ShotContext {
        buses,
        bus,
        sounds,
        clock,
    } = ctx;
    let (bus, clock) = (*bus, *clock);
    let Some(shot) = layer.shot.as_mut() else {
        return false;
    };
    if clock < shot.cooldown_until {
        shot.skipped += 1;
        return false;
    }
    let count = def.sounds.len().max(1);
    let index = shot.next % count;
    shot.next = (index + 1) % count;
    let Some(asset) = sounds.get(&(layer_def.name.clone(), index)) else {
        shot.skipped += 1;
        return false;
    };
    let params = PlaybackParams {
        bus: Some(bus.to_string()),
        volume_db: out.volume_db,
        pitch: out.pitch.max(0.0),
        ..PlaybackParams::default()
    };
    let played = asset
        .play_data(&params)
        .and_then(|data| match layer.track.as_mut() {
            Some(track) => play_data_on_track(track, data),
            None => play_data_on(buses, bus, data),
        });
    let Some(shot) = layer.shot.as_mut() else {
        return false;
    };
    match played {
        Ok(_instance) => {
            // Fire and forget: the shot plays out (a despawn does not cut it; the filter track
            // persists until its sounds finish).
            shot.fired += 1;
            shot.cooldown_until = clock + f64::from(def.cooldown_secs.max(0.0));
            true
        }
        Err(error) => {
            shot.skipped += 1;
            tracing::warn!(target: "regia_audio", "patch layer `{}`: one-shot failed: {error}", layer_def.name);
            false
        }
    }
}

/// Fires a one-shot layer as often as its trigger crossed since the last tick.
pub(super) fn tick_one_shot(
    ctx: &mut ShotContext<'_>,
    layer_def: &PatchLayerDef,
    layer: &mut PatchLayerRuntime,
    params: &HashMap<String, SmoothedParam>,
    out: &LayerOutputs,
) {
    let Some(def) = layer_def.one_shot.as_ref() else {
        return;
    };
    let Some(shot) = layer.shot.as_mut() else {
        return;
    };
    let Some(value) = def
        .trigger
        .parameter()
        .and_then(|name| params.get(name))
        .map(|p| p.value)
    else {
        return;
    };
    let fires = def.trigger.fires(shot.last, value);
    shot.last = value;
    for _ in 0..fires {
        fire(ctx, layer_def, def, layer, out);
    }
}

/// Pushes a granular layer's current controls to its voice.
pub(super) fn tick_granular(
    layer_def: &PatchLayerDef,
    layer: &mut PatchLayerRuntime,
    params: &HashMap<String, SmoothedParam>,
    out: &LayerOutputs,
) {
    let (Some(def), Some(handle)) = (layer_def.granular.as_ref(), layer.granular.as_ref()) else {
        return;
    };
    handle.set_controls(controls_for(def, params, out));
}

/// Retargets a layer's low-pass to its current cutoff (the loop path does the same inline).
pub(super) fn retarget_filter(layer: &mut PatchLayerRuntime, out: &LayerOutputs, micro: Duration) {
    if let Some(cutoff) = out.cutoff_hz
        && let Some(filter) = layer.filter.as_mut()
        && layer.applied.cutoff_hz != Some(cutoff)
    {
        filter.set_cutoff(cutoff, tween(micro));
        layer.applied.cutoff_hz = Some(cutoff);
    }
}

/// The grain controls of a granular layer: position and density from its curves, pitch and
/// linear gain from the layer outputs.
fn controls_for(
    def: &PatchGranularDef,
    params: &HashMap<String, SmoothedParam>,
    out: &LayerOutputs,
) -> GranularControls {
    let (position, density_hz) = granular_controls(def, params);
    GranularControls {
        position,
        pitch: out.pitch.max(0.0),
        density_hz,
        gain: db_to_gain(finite_db(out.volume_db)),
    }
}

/// One parameter of a spawned patch (`audio.patch_state`).
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct PatchParamInfo {
    /// The parameter name.
    pub name: String,
    /// The smoothed value now.
    pub value: f32,
    /// The target it slews toward.
    pub target: f32,
    /// `Component.field` when bound to a reflected field, else `None`.
    pub binding: Option<String>,
}

/// One layer of a spawned patch (`audio.patch_state`).
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct PatchLayerInfo {
    /// The layer name.
    pub name: String,
    /// `loop`, `keys`, `one_shot` or `granular`.
    pub kind: &'static str,
    /// The layer volume now, dB (base plus curves).
    pub volume_db: f32,
    /// The layer pitch ratio now.
    pub pitch: f32,
    /// One-shot layers: shots played.
    pub fired: u64,
    /// One-shot layers: shots skipped (cooldown or a refused sound).
    pub skipped: u64,
    /// Granular layers: grains started (as of the last rendered block).
    pub grains: u64,
    /// Granular layers: grains skipped because the pool was full.
    pub grains_dropped: u64,
}

/// A spawned patch (`audio.patch_state`).
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct PatchInfo {
    /// The patch id from [`Audio::spawn_patch`].
    pub id: u64,
    /// The patch name.
    pub name: String,
    /// The bus it plays on.
    pub bus: String,
    /// Its parameters, in declaration order.
    pub parameters: Vec<PatchParamInfo>,
    /// Its layers, in declaration order.
    pub layers: Vec<PatchLayerInfo>,
}

impl Audio {
    /// The decoded frames of `asset` at the output's sample rate (resampled when needed).
    fn frames_at_output_rate(&self, asset: &SoundAsset) -> Result<Arc<[kira::Frame]>, AudioError> {
        let decoded = asset.decode_to_memory()?;
        let (frames, source_rate) = decoded
            .static_frames()
            .ok_or_else(|| AudioError::load(asset.label(), "no decoded frames"))?;
        let output_rate = self
            .mixdown()
            .map_or(self.output_sample_rate, |job| job.sample_rate);
        if source_rate == output_rate {
            return Ok(Arc::from(frames));
        }
        let left: Vec<f32> = frames.iter().map(|f| f.left).collect();
        let right: Vec<f32> = frames.iter().map(|f| f.right).collect();
        let left = crate::resample_sinc(&left, source_rate, output_rate);
        let right = crate::resample_sinc(&right, source_rate, output_rate);
        Ok(left
            .iter()
            .zip(&right)
            .map(|(l, r)| kira::Frame { left: *l, right: *r })
            .collect())
    }

    /// Starts a granular layer's voice on its filter track, or on `bus`.
    pub(super) fn start_granular(
        &mut self,
        asset: &SoundAsset,
        def: &PatchGranularDef,
        initial: &LayerOutputs,
        params: &HashMap<String, SmoothedParam>,
        track: Option<&mut TrackHandle>,
        bus: &str,
    ) -> Result<GranularHandle, AudioError> {
        let frames = self.frames_at_output_rate(asset)?;
        let output_rate = self
            .mixdown()
            .map_or(self.output_sample_rate, |job| job.sample_rate);
        let mut initial = initial.clone();
        initial.volume_db += asset.loudness_gain_db();
        let data = GranularSoundData::new(
            frames,
            output_rate,
            def.grain_secs,
            def.position_jitter,
            def.seed,
            controls_for(def, params, &initial),
        );
        let played = match track {
            Some(track) => track.play(data),
            None => self
                .buses
                .track(bus)
                .ok_or_else(|| AudioError::UnknownBus(bus.to_string()))?
                .play(data),
        };
        played.map_err(|e| AudioError::PlayFailed(format!("{e:?}")))
    }

    /// Fires the one-shot layer `layer` of patch `id` now, whatever its trigger (cooldown still
    /// applies). Returns whether a shot started.
    ///
    /// # Errors
    /// [`AudioError::UnknownEntry`] for an unknown patch or layer; [`AudioError::Invalid`] when the
    /// layer is not a one-shot layer.
    pub fn patch_trigger(&mut self, id: u64, layer: &str) -> Result<bool, AudioError> {
        let Audio { patches, buses, .. } = self;
        let Some(player) = patches.iter_mut().find(|p| p.id == id) else {
            return Err(AudioError::UnknownEntry(format!("patch {id}")));
        };
        let Some(index) = player.patch.def.layers.iter().position(|l| l.name == layer) else {
            return Err(AudioError::UnknownEntry(format!(
                "patch {id} has no layer `{layer}`"
            )));
        };
        let layer_def = &player.patch.def.layers[index];
        let Some(def) = layer_def.one_shot.as_ref() else {
            return Err(AudioError::Invalid(format!(
                "patch layer `{layer}` is not a one-shot layer"
            )));
        };
        let out = crate::patch::layer_outputs(layer_def, &player.params);
        let Some(runtime) = player.layers.get_mut(index) else {
            return Err(AudioError::UnknownEntry(format!(
                "patch {id} has no layer `{layer}`"
            )));
        };
        let mut ctx = ShotContext {
            buses,
            bus: &player.bus,
            sounds: &player.patch.key_sounds,
            clock: player.clock_secs,
        };
        Ok(fire(&mut ctx, layer_def, def, runtime, &out))
    }

    /// The definition of spawned patch `id`.
    pub fn patch_def(&self, id: u64) -> Option<&crate::AudioPatchDef> {
        self.patches.iter().find(|p| p.id == id).map(|p| &p.patch.def)
    }

    /// Sets the target of the `index`-th declared parameter of patch `id` (the binding system).
    /// Returns false for an unknown patch or index.
    pub(crate) fn patch_set_param_at(&mut self, id: u64, index: usize, value: f32) -> bool {
        let Some(player) = self.patches.iter_mut().find(|p| p.id == id) else {
            return false;
        };
        let Some(name) = player.patch.def.parameters.get(index).map(|p| p.name.as_str()) else {
            return false;
        };
        match player.params.get_mut(name) {
            Some(param) => {
                param.set_target(value);
                true
            }
            None => false,
        }
    }

    /// The state of every spawned patch: parameters, layer outputs, one-shot and grain counts.
    pub fn patch_state(&self) -> Vec<PatchInfo> {
        self.patches
            .iter()
            .map(|player| PatchInfo {
                id: player.id,
                name: player.patch.def.name.clone(),
                bus: player.bus.clone(),
                parameters: player
                    .patch
                    .def
                    .parameters
                    .iter()
                    .map(|def| {
                        let param = player.params.get(&def.name);
                        PatchParamInfo {
                            name: def.name.clone(),
                            value: param.map_or(def.min, |p| p.value),
                            target: param.map_or(def.min, |p| p.target),
                            binding: def
                                .binding
                                .as_ref()
                                .map(|b| format!("{}.{}", b.component, b.field)),
                        }
                    })
                    .collect(),
                layers: player
                    .patch
                    .def
                    .layers
                    .iter()
                    .zip(&player.layers)
                    .map(|(def, runtime)| {
                        let out = crate::patch::layer_outputs(def, &player.params);
                        let shot = runtime.shot.as_ref();
                        let grains = runtime.granular.as_ref();
                        PatchLayerInfo {
                            name: def.name.clone(),
                            kind: runtime.kind.as_str(),
                            volume_db: out.volume_db,
                            pitch: out.pitch,
                            fired: shot.map_or(0, |s| s.fired),
                            skipped: shot.map_or(0, |s| s.skipped),
                            grains: grains.map_or(0, GranularHandle::grains_started),
                            grains_dropped: grains.map_or(0, GranularHandle::grains_dropped),
                        }
                    })
                    .collect(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use crate::patch::PatchTriggerDef;

    #[test]
    fn triggers_fire_on_crossings_only() {
        let rising = PatchTriggerDef::Rising {
            parameter: "p".into(),
            threshold: 0.5,
        };
        assert_eq!(rising.fires(0.4, 0.5), 1, "reaching the threshold fires");
        assert_eq!(rising.fires(0.5, 0.9), 0, "already at it");
        assert_eq!(rising.fires(0.9, 0.1), 0, "falling past it");
        let falling = PatchTriggerDef::Falling {
            parameter: "p".into(),
            threshold: 0.5,
        };
        assert_eq!(falling.fires(0.6, 0.5), 1);
        assert_eq!(falling.fires(0.1, 0.9), 0);
        let every = PatchTriggerDef::Every {
            parameter: "p".into(),
            step: 1.0,
        };
        assert_eq!(every.fires(0.2, 0.9), 0);
        assert_eq!(every.fires(0.9, 1.1), 1);
        assert_eq!(every.fires(3.5, 1.5), 2, "down across 3 and 2");
        assert_eq!(
            every.fires(0.0, 100.0),
            crate::patch::MAX_FIRES_PER_TICK,
            "capped"
        );
        assert_eq!(every.fires(f32::NAN, 1.0), 0);
        assert_eq!(PatchTriggerDef::Manual.fires(0.0, 1.0), 0);
        let bad = PatchTriggerDef::Every {
            parameter: "p".into(),
            step: 0.0,
        };
        assert_eq!(bad.fires(0.0, 5.0), 0);
    }
}
