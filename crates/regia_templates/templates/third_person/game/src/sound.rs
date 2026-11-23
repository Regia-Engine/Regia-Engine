// SPDX-License-Identifier: MIT-0
//! Sound hooks. Gameplay writes a [`SoundCue`]; [`play_cues`] plays it on the `Sfx` bus.
//!
//! Every sound here is synthesized in memory (a decaying tone and noise), so the template ships
//! no audio files. To use recorded sound, build a `SoundAsset` from your file instead of the
//! `synth` calls in [`build_sounds`]; the cues and everything that writes them stay the same.
//! There is also a looping ambient bed on the `Ambience` bus (a `LoopGenDef` graph, [`AMBIENT`]).
//! Volumes come from the settings menu through `regia::framework::settings::SettingsPlugin`.
//! A failed sound is silent, never fatal; [`SoundStats`] counts what played so agents and tests
//! can check sound without a speaker.

use regia::audio::{Audio, AudioEmitter, LoopGenDef, LoopRegion, PlaybackParams, SoundAsset, SoundClip};
use regia::prelude::*;
use regia_core::math::Vec3;
use regia_framework::character::{Character, CharacterState};
use regia_scene::Transform;

/// Meters between two footsteps.
pub const STRIDE: f32 = 1.4;

/// The ambient bed: soft wind over a low rumble.
pub const AMBIENT: &str = r#"(
    version: 1,
    name: "template_wind_bed",
    sample_rate: 48000,
    length_secs: 6.0,
    channels: 2,
    seed: 2522,
    crossfade_secs: 0.5,
    output_gain_db: -16.0,
    nodes: [
        (id: "air", node: Noise(color: Pink, gain: 0.6)),
        (id: "air_lp", node: Filter(input: "air", mode: LowPass, cutoff_hz: 900.0, q: 0.6)),
        (id: "gusts", node: Am(input: "air_lp", rate_hz: 0.17, depth: 0.6)),
        (id: "low", node: Rumble(cutoff_hz: 70.0, q: 0.8, gain: 0.25)),
        (id: "bed", node: Mix(inputs: [(input: "gusts"), (input: "low", gain: 0.6)])),
    ],
    output: "bed",
)"#;

/// A sound the game asks for.
#[derive(Message, Debug, Clone, Copy, PartialEq)]
pub enum SoundCue {
    /// A sword swing.
    Swing(Vec3),
    /// Something was hit.
    Hit(Vec3),
    /// Something fell.
    Fall(Vec3),
    /// A coin was picked up.
    Pickup(Vec3),
}

/// The synthesized clips.
#[derive(Resource, Default)]
pub struct Sounds {
    step: Option<SoundAsset>,
    swing: Option<SoundAsset>,
    hit: Option<SoundAsset>,
    fall: Option<SoundAsset>,
    pickup: Option<SoundAsset>,
    ambient: Option<SoundAsset>,
}

/// What played (reflected: sound has no other visible surface headless).
#[derive(Resource, Reflect, Debug, Clone, Default, PartialEq)]
#[reflect(Resource, Default)]
pub struct SoundStats {
    /// Footsteps played.
    pub footsteps: u32,
    /// Cues played (swings, hits, falls, pickups).
    pub cues: u32,
    /// The ambient bed is playing.
    pub ambient_playing: bool,
    /// Problems building the sounds.
    pub errors: Vec<String>,
}

/// Distance walked since the last footstep.
#[derive(Component, Debug, Clone, Copy, Default)]
pub struct StepClock {
    walked: f32,
    last: Option<Vec3>,
}

/// Installs the sounds. An app with no audio plugin gets a device-less one so tests can run.
pub struct SoundPlugin;

impl Plugin for SoundPlugin {
    fn build(&self, app: &mut App) {
        if !app.has_plugin(std::any::type_name::<regia::audio::AudioPlugin>()) {
            if !app.world().contains_resource::<regia::audio::AudioSettings>() {
                app.insert_resource(regia::audio::AudioSettings {
                    headless: true,
                    ..Default::default()
                });
            }
            app.add_plugins(regia::audio::AudioPlugin);
        }
        let (sounds, errors) = build_sounds();
        app.insert_resource(sounds)
            .insert_resource(SoundStats {
                errors,
                ..Default::default()
            })
            .register_type::<SoundStats>()
            .add_message::<SoundCue>()
            .add_systems(Update, (start_ambient, footsteps, play_cues));
    }
}

/// A small deterministic noise source (the sounds need no quality randomness).
fn noise(i: u32, salt: u32) -> f32 {
    let x = i
        .wrapping_mul(747_796_405)
        .wrapping_add(2_891_336_453 ^ salt.wrapping_mul(2_654_435_761));
    let x = (x ^ (x >> 16)).wrapping_mul(0x45d9_f3b5);
    ((x >> 9) as f32 / (u32::MAX >> 9) as f32) * 2.0 - 1.0
}

/// A clip from `seconds` of `f(i, t)` at 48 kHz, or `None` for an invalid buffer.
fn synth(seconds: f32, f: impl Fn(u32, f32) -> f32) -> Option<SoundClip> {
    let n = (seconds * 48_000.0) as u32;
    let samples: Vec<f32> = (0..n)
        .map(|i| f(i, i as f32 / 48_000.0).clamp(-1.0, 1.0))
        .collect();
    SoundClip::new(samples, 48_000).ok()
}

fn asset(clip: Option<SoundClip>, name: &str, errors: &mut Vec<String>) -> Option<SoundAsset> {
    let asset = clip.map(|clip| SoundAsset::from_clip(&clip));
    if asset.is_none() {
        errors.push(format!("{name}: could not build the clip"));
    }
    asset
}

/// Builds every clip and the ambient loop; returns the problems.
#[must_use]
pub fn build_sounds() -> (Sounds, Vec<String>) {
    use std::f32::consts::{PI, TAU};
    let mut errors = Vec::new();
    let step = synth(0.09, |i, t| {
        let env = (-t * 55.0).exp();
        (noise(i / 3, 1) * 0.5 + (t * 95.0 * TAU).sin() * 0.6) * env * 0.7
    });
    let swing = synth(0.22, |i, t| {
        let env = (t / 0.22 * PI).sin();
        noise(i / 2, 9) * env * env * 0.35
    });
    let hit = synth(0.18, |i, t| {
        let click = if t < 0.004 { noise(i, 3) } else { 0.0 };
        ((t * 140.0 * TAU).sin() * 0.8 + noise(i, 5) * 0.25) * (-t * 28.0).exp() + click * 0.6
    });
    let fall = synth(0.5, |i, t| {
        ((t * 48.0 * TAU).sin() * 0.7 + noise(i / 8, 11) * 0.4) * (-t * 7.0).exp()
    });
    let pickup = synth(0.25, |_, t| (t * 880.0 * TAU).sin() * (-t * 14.0).exp() * 0.5);
    let ambient = match LoopGenDef::parse(AMBIENT).and_then(|def| def.render()) {
        Ok(rendered) => Some(rendered.to_sound_asset()),
        Err(err) => {
            errors.push(format!("ambient: {err}"));
            None
        }
    };
    let sounds = Sounds {
        step: asset(step, "step", &mut errors),
        swing: asset(swing, "swing", &mut errors),
        hit: asset(hit, "hit", &mut errors),
        fall: asset(fall, "fall", &mut errors),
        pickup: asset(pickup, "pickup", &mut errors),
        ambient,
    };
    (sounds, errors)
}

fn on_sfx() -> PlaybackParams {
    PlaybackParams {
        bus: Some("Sfx".to_owned()),
        ..PlaybackParams::default()
    }
}

/// Plays `asset` on the `Sfx` bus at `at` (nothing happens without an audio engine).
fn play_sfx(world: &mut World, asset: &SoundAsset, at: Vec3) {
    let Some(mut audio) = world.get_resource_mut::<Audio>() else {
        return;
    };
    let emitter = AudioEmitter {
        min_distance: 2.0,
        max_distance: 40.0,
        bus: Some("Sfx".into()),
        ..AudioEmitter::default()
    };
    if let Err(err) = audio.play_asset_at(asset, on_sfx(), at, &emitter) {
        debug!("sound not played: {err}");
    }
}

/// `Update`: starts the ambient bed once, looped on the `Ambience` bus.
pub fn start_ambient(world: &mut World, mut tried: Local<bool>) {
    if *tried || !world.contains_resource::<Audio>() {
        return;
    }
    *tried = true;
    let Some(asset) = world.get_resource::<Sounds>().and_then(|s| s.ambient.clone()) else {
        return;
    };
    let params = PlaybackParams {
        bus: Some("Ambience".to_owned()),
        loop_region: Some(LoopRegion {
            start_secs: 0.0,
            end_secs: None,
        }),
        ..PlaybackParams::default()
    };
    let started = world
        .get_resource_mut::<Audio>()
        .is_some_and(|mut audio| audio.start_asset(&asset, params).is_ok());
    if let Some(mut stats) = world.get_resource_mut::<SoundStats>() {
        stats.ambient_playing = started;
    }
}

/// `Update`: a footstep every [`STRIDE`] meters the grounded player walks.
#[tracing::instrument(skip_all, name = "sound_footsteps")]
pub fn footsteps(world: &mut World) {
    let walkers: Vec<(Entity, Vec3, bool)> = world
        .query_filtered::<(Entity, &Transform, &CharacterState), With<Character>>()
        .iter(world)
        .map(|(entity, transform, state)| (entity, transform.translation, state.grounded))
        .collect();
    for (entity, at, grounded) in walkers {
        let mut clock = world.get::<StepClock>(entity).copied().unwrap_or_default();
        let moved = clock
            .last
            .map_or(0.0, |last| Vec3::new(at.x - last.x, 0.0, at.z - last.z).length());
        clock.last = Some(at);
        // A teleport (a respawn, a load) is not a walk.
        if grounded && moved < 1.0 {
            clock.walked += moved;
        }
        let step = clock.walked >= STRIDE;
        if step {
            clock.walked -= STRIDE;
        }
        world.entity_mut(entity).insert(clock);
        if step && let Some(asset) = world.get_resource::<Sounds>().and_then(|s| s.step.clone()) {
            play_sfx(world, &asset, at);
            if let Some(mut stats) = world.get_resource_mut::<SoundStats>() {
                stats.footsteps += 1;
            }
        }
    }
}

/// `Update`: plays the gameplay's [`SoundCue`]s.
#[tracing::instrument(skip_all, name = "sound_cues")]
pub fn play_cues(world: &mut World, mut cursor: Local<regia::ecs::ecs::message::MessageCursor<SoundCue>>) {
    let cues: Vec<SoundCue> = world
        .get_resource::<Messages<SoundCue>>()
        .map(|messages| cursor.read(messages).copied().collect())
        .unwrap_or_default();
    for cue in cues {
        let (clip, at) = match cue {
            SoundCue::Swing(at) => (world.get_resource::<Sounds>().and_then(|s| s.swing.clone()), at),
            SoundCue::Hit(at) => (world.get_resource::<Sounds>().and_then(|s| s.hit.clone()), at),
            SoundCue::Fall(at) => (world.get_resource::<Sounds>().and_then(|s| s.fall.clone()), at),
            SoundCue::Pickup(at) => (world.get_resource::<Sounds>().and_then(|s| s.pickup.clone()), at),
        };
        if let Some(clip) = clip {
            play_sfx(world, &clip, at);
        }
        if let Some(mut stats) = world.get_resource_mut::<SoundStats>() {
            stats.cues += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_sound_builds() {
        let (sounds, errors) = build_sounds();
        assert!(errors.is_empty(), "{errors:?}");
        assert!(sounds.swing.is_some() && sounds.hit.is_some() && sounds.ambient.is_some());
    }
}
