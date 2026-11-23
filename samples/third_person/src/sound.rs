// SPDX-License-Identifier: MIT-0
//! Sound (ADR-2596): every sound is procedural, so the sample ships no audio files.
//!
//! - **Beds**: an ambient wind bed and a quiet music pad, `.loopgen.ron` graphs
//!   (`assets/audio/*.loopgen.ron`) rendered in memory by `regia_audio::LoopGenDef` and looped on
//!   the `Ambience` and `Music` buses from the first frame of play.
//! - **One-shots**: footsteps (three variants), the swing, hits and the fall are short synthesized
//!   buffers (`SoundClip`s from decaying noise and tones, a small integer noise generator: no
//!   `rand`), played on the `Sfx` bus at their world position.
//!
//! Footsteps follow the distance a grounded character covers; each step also writes a
//! `HearingEvent` for the enemies (sprinting carries further). The settings menu's master,
//! music and effects volumes reach the `Master`, `Music` and `Sfx` buses through
//! `regia_framework::settings::SettingsPlugin`.

use regia::prelude::*;
use regia_audio::{Audio, AudioEmitter, LoopGenDef, LoopRegion, PlaybackParams, SoundAsset, SoundClip};
use regia_core::math::Vec3;
use regia_ecs::ecs::message::{MessageCursor, Messages};
use regia_framework::character::{Character, CharacterInput, CharacterState};
use regia_scene::Transform;

use crate::combat::{HitLanded, TEAM_ENEMY, TEAM_PLAYER};

/// The ambient bed graph.
pub const AMBIENT_LOOP_TEXT: &str = include_str!("../assets/audio/ambient.loopgen.ron");
/// The music pad graph.
pub const MUSIC_LOOP_TEXT: &str = include_str!("../assets/audio/music.loopgen.ron");

/// Meters between two footsteps while walking.
pub const STRIDE: f32 = 1.4;

/// A sound the game asks for (gameplay writes these; [`play_cues`] plays them).
#[derive(Message, Debug, Clone, Copy, PartialEq)]
pub enum SoundCue {
    /// A sword swing at a position.
    Swing(Vec3),
}

/// The synthesized clips.
#[derive(Resource)]
pub struct SampleSounds {
    /// Footstep variants (a variant that did not build is `None` and stays silent).
    pub steps: [Option<SoundAsset>; 3],
    /// A sword swing.
    pub swing: Option<SoundAsset>,
    /// A hit on an enemy.
    pub hit_enemy: Option<SoundAsset>,
    /// A hit on the player.
    pub hit_player: Option<SoundAsset>,
    /// A body falling.
    pub fall: Option<SoundAsset>,
    /// The ambient bed (`None` when the graph did not render).
    pub ambient: Option<SoundAsset>,
    /// The music pad.
    pub music: Option<SoundAsset>,
}

/// What played (reflected for agent checks: sound has no other visible surface headless).
#[derive(Resource, Reflect, Debug, Clone, Default, PartialEq)]
#[reflect(Resource, Default)]
pub struct SoundStats {
    /// Footsteps played.
    pub footsteps: u32,
    /// Swings played.
    pub swings: u32,
    /// Hit sounds played.
    pub hits: u32,
    /// Fall sounds played.
    pub falls: u32,
    /// The ambient bed is playing.
    pub ambient_playing: bool,
    /// The music pad is playing.
    pub music_playing: bool,
    /// Frames of the rendered ambient loop (0 when it did not render).
    pub ambient_frames: u32,
    /// Problems building the sounds.
    pub errors: Vec<String>,
}

/// Per-character distance since the last footstep.
#[derive(Component, Debug, Clone, Copy, Default, PartialEq)]
pub struct StepClock {
    /// Meters walked since the last step.
    pub walked: f32,
    /// Position last frame.
    pub last: Option<Vec3>,
    /// Steps taken (picks the variant).
    pub count: u32,
}

/// Installs the sounds.
pub struct SoundPlugin;

impl Plugin for SoundPlugin {
    fn build(&self, app: &mut App) {
        let (sounds, errors) = build_sounds();
        let ambient_frames = sounds.ambient.as_ref().map_or(0, |_| ambient_frames());
        app.insert_resource(sounds)
            .insert_resource(SoundStats {
                errors,
                ambient_frames,
                ..Default::default()
            })
            .register_type::<SoundStats>()
            .add_message::<SoundCue>()
            .add_systems(Update, (start_beds, footsteps, play_cues, play_hits, play_falls));
    }
}

/// A tiny deterministic noise source (an LCG; the sounds need no quality randomness).
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

/// A footstep: a short low-passed noise thump; `variant` changes the noise and the pitch.
fn footstep(variant: u32) -> Option<SoundClip> {
    let pitch = 1.0 + variant as f32 * 0.08;
    synth(0.09, move |i, t| {
        let env = (-t * 55.0 * pitch).exp();
        let thump = (t * 95.0 * pitch * std::f32::consts::TAU).sin() * 0.6;
        (noise(i / 3, variant + 1) * 0.5 + thump) * env * 0.7
    })
}

/// A swing: a noise whoosh with a rising then falling envelope.
fn swing() -> Option<SoundClip> {
    synth(0.22, |i, t| {
        let env = (t / 0.22 * std::f32::consts::PI).sin();
        noise(i / 2, 9) * env * env * 0.35
    })
}

/// A hit: a sharp click on a low tone (`low` = the player being hit, duller).
fn hit(low: bool) -> Option<SoundClip> {
    let hz = if low { 70.0 } else { 140.0 };
    synth(0.18, move |i, t| {
        let env = (-t * 28.0).exp();
        let click = if t < 0.004 { noise(i, 3) } else { 0.0 };
        ((t * hz * std::f32::consts::TAU).sin() * 0.8 + noise(i, 5) * 0.25) * env + click * 0.6
    })
}

/// A fall: a long low rumble.
fn fall() -> Option<SoundClip> {
    synth(0.5, |i, t| {
        let env = (-t * 7.0).exp();
        ((t * 48.0 * std::f32::consts::TAU).sin() * 0.7 + noise(i / 8, 11) * 0.4) * env
    })
}

fn asset(clip: Option<SoundClip>, name: &str, errors: &mut Vec<String>) -> Option<SoundAsset> {
    let asset = clip.map(|clip| SoundAsset::from_clip(&clip));
    if asset.is_none() {
        errors.push(format!("{name}: could not build the clip"));
    }
    asset
}

fn render_loop(text: &str, name: &str, errors: &mut Vec<String>) -> Option<SoundAsset> {
    match LoopGenDef::parse(text).and_then(|def| def.render()) {
        Ok(rendered) => Some(rendered.to_sound_asset()),
        Err(err) => {
            errors.push(format!("{name}: {err}"));
            None
        }
    }
}

/// Frames of the rendered ambient loop.
fn ambient_frames() -> u32 {
    LoopGenDef::parse(AMBIENT_LOOP_TEXT)
        .and_then(|def| def.render())
        .map_or(0, |r| r.frames() as u32)
}

/// Builds every clip and both loops; returns the problems (a failed sound is silent, never fatal).
pub fn build_sounds() -> (SampleSounds, Vec<String>) {
    let mut errors = Vec::new();
    let steps = [
        asset(footstep(0), "footstep_0", &mut errors),
        asset(footstep(1), "footstep_1", &mut errors),
        asset(footstep(2), "footstep_2", &mut errors),
    ];
    let sounds = SampleSounds {
        steps,
        swing: asset(swing(), "swing", &mut errors),
        hit_enemy: asset(hit(false), "hit_enemy", &mut errors),
        hit_player: asset(hit(true), "hit_player", &mut errors),
        fall: asset(fall(), "fall", &mut errors),
        ambient: render_loop(AMBIENT_LOOP_TEXT, "ambient.loopgen.ron", &mut errors),
        music: render_loop(MUSIC_LOOP_TEXT, "music.loopgen.ron", &mut errors),
    };
    (sounds, errors)
}

fn on_bus(bus: &str) -> PlaybackParams {
    PlaybackParams {
        bus: Some(bus.to_owned()),
        ..PlaybackParams::default()
    }
}

/// Plays `asset` on the `Sfx` bus at `at`.
fn play_sfx(world: &mut World, asset: &SoundAsset, at: Vec3) {
    if let Some(mut audio) = world.get_resource_mut::<Audio>() {
        let emitter = AudioEmitter {
            min_distance: 2.0,
            max_distance: 40.0,
            bus: Some("Sfx".into()),
            ..AudioEmitter::default()
        };
        if let Err(err) = audio.play_asset_at(asset, on_bus("Sfx"), at, &emitter) {
            tracing::debug!(target: "third_person", "sound not played: {err}");
        }
    }
}

/// `Update`: starts the ambient bed and the music pad once (looped). A refused start is
/// recorded in [`SoundStats::errors`] and not retried.
#[tracing::instrument(skip_all, target = "third_person", name = "sound_beds")]
pub fn start_beds(world: &mut World) {
    if world.contains_resource::<BedsTried>() || !world.contains_resource::<Audio>() {
        return;
    }
    world.insert_resource(BedsTried);
    let (ambient, music) = match world.get_resource::<SampleSounds>() {
        Some(s) => (s.ambient.clone(), s.music.clone()),
        None => return,
    };
    let looped = |bus: &str| PlaybackParams {
        loop_region: Some(LoopRegion {
            start_secs: 0.0,
            end_secs: None,
        }),
        ..on_bus(bus)
    };
    let mut ambient_ok = false;
    let mut music_ok = false;
    if let Some(mut audio) = world.get_resource_mut::<Audio>() {
        if let Some(asset) = &ambient {
            ambient_ok = audio.start_asset(asset, looped("Ambience")).is_ok();
        }
        if let Some(asset) = &music {
            music_ok = audio.start_asset(asset, looped("Music")).is_ok();
        }
    }
    if let Some(mut stats) = world.get_resource_mut::<SoundStats>() {
        stats.ambient_playing = ambient_ok;
        stats.music_playing = music_ok;
        if !ambient_ok || !music_ok {
            stats
                .errors
                .push(format!("beds refused: ambient {ambient_ok}, music {music_ok}"));
        }
    }
}

/// The beds were started (or refused); they are not tried again.
#[derive(Resource)]
struct BedsTried;

/// `Update`: a footstep each [`STRIDE`] meters a grounded character walks (sprint: longer
/// strides, louder for the enemies).
#[tracing::instrument(skip_all, target = "third_person", name = "sound_footsteps")]
pub fn footsteps(world: &mut World) {
    let walkers: Vec<(Entity, Vec3, bool, bool)> = world
        .query_filtered::<(Entity, &Transform, &CharacterState, &CharacterInput), With<Character>>()
        .iter(world)
        .map(|(e, t, s, i)| (e, t.translation, s.grounded, i.sprint))
        .collect();
    for (character, at, grounded, sprint) in walkers {
        if world.get::<StepClock>(character).is_none() {
            world.entity_mut(character).insert(StepClock::default());
        }
        let Some(mut clock) = world.get::<StepClock>(character).copied() else {
            continue;
        };
        let moved = clock
            .last
            .map_or(0.0, |last| Vec3::new(at.x - last.x, 0.0, at.z - last.z).length());
        clock.last = Some(at);
        // A teleport (respawn, an agent placing the character) is not a walk.
        if grounded && moved < 1.0 && !crate::combat::is_down(world, character) {
            clock.walked += moved;
        }
        let stride = if sprint { STRIDE * 1.4 } else { STRIDE };
        let step = clock.walked >= stride;
        if step {
            clock.walked -= stride;
            clock.count += 1;
        }
        let variant = (clock.count % 3) as usize;
        world.entity_mut(character).insert(clock);
        if step {
            if let Some(asset) = world
                .get_resource::<SampleSounds>()
                .and_then(|s| s.steps[variant].clone())
            {
                play_sfx(world, &asset, at);
            }
            if let Some(mut stats) = world.get_resource_mut::<SoundStats>() {
                stats.footsteps += 1;
            }
            world.write_message(regia_ai::HearingEvent {
                position: at,
                radius: if sprint { 9.0 } else { 5.0 },
                team: TEAM_ENEMY,
                source: character,
            });
        }
    }
}

/// `Update`: plays the gameplay's [`SoundCue`]s.
#[tracing::instrument(skip_all, target = "third_person", name = "sound_cues")]
pub fn play_cues(world: &mut World, mut cursor: Local<MessageCursor<SoundCue>>) {
    let cues: Vec<SoundCue> = world
        .get_resource::<Messages<SoundCue>>()
        .map(|m| cursor.read(m).copied().collect())
        .unwrap_or_default();
    for cue in cues {
        match cue {
            SoundCue::Swing(at) => {
                if let Some(asset) = world.get_resource::<SampleSounds>().and_then(|s| s.swing.clone()) {
                    play_sfx(world, &asset, at);
                }
                if let Some(mut stats) = world.get_resource_mut::<SoundStats>() {
                    stats.swings += 1;
                }
            }
        }
    }
}

/// `Update`: a hit sound for every [`HitLanded`].
#[tracing::instrument(skip_all, target = "third_person", name = "sound_hits")]
pub fn play_hits(world: &mut World, mut cursor: Local<MessageCursor<HitLanded>>) {
    let hits: Vec<HitLanded> = world
        .get_resource::<Messages<HitLanded>>()
        .map(|m| cursor.read(m).copied().collect())
        .unwrap_or_default();
    for hit in hits {
        let asset = world.get_resource::<SampleSounds>().and_then(|s| {
            if hit.target_team == TEAM_PLAYER {
                s.hit_player.clone()
            } else {
                s.hit_enemy.clone()
            }
        });
        if let Some(asset) = asset {
            play_sfx(world, &asset, hit.position);
        }
        if let Some(mut stats) = world.get_resource_mut::<SoundStats>() {
            stats.hits += 1;
        }
    }
}

/// `Update`: a fall sound for every new corpse.
#[tracing::instrument(skip_all, target = "third_person", name = "sound_falls")]
pub fn play_falls(world: &mut World) {
    let fresh: Vec<(Entity, Vec3)> = world
        .query_filtered::<(Entity, &Transform), (With<crate::ragdoll_fx::Corpse>, Without<FallHeard>)>()
        .iter(world)
        .map(|(e, t)| (e, t.translation))
        .collect();
    for (corpse, at) in fresh {
        world.entity_mut(corpse).insert(FallHeard);
        if let Some(asset) = world.get_resource::<SampleSounds>().and_then(|s| s.fall.clone()) {
            play_sfx(world, &asset, at);
        }
        if let Some(mut stats) = world.get_resource_mut::<SoundStats>() {
            stats.falls += 1;
        }
    }
}

/// Marker: the corpse's fall sound played.
#[derive(Component)]
pub struct FallHeard;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_sound_builds_and_both_loops_render() {
        let (sounds, errors) = build_sounds();
        assert!(errors.is_empty(), "{errors:?}");
        assert!(sounds.steps.iter().all(Option::is_some));
        assert!(sounds.swing.is_some() && sounds.hit_enemy.is_some() && sounds.hit_player.is_some());
        assert!(sounds.fall.is_some() && sounds.ambient.is_some() && sounds.music.is_some());
    }

    #[test]
    fn the_loops_are_seamless_and_not_silent() {
        for text in [AMBIENT_LOOP_TEXT, MUSIC_LOOP_TEXT] {
            let rendered = LoopGenDef::parse(text).and_then(|d| d.render()).expect("renders");
            let stats = rendered.stats();
            assert!(stats.rms > 1e-4, "audible: {stats:?}");
            assert!(stats.peak <= 1.0);
            // The documented seam bound (docs/formats/loopgen-ron.md).
            assert!(stats.seam_jump <= 1.1 * stats.max_step + 0.0002, "{stats:?}");
        }
    }

    #[test]
    fn synthesized_clips_are_bounded_and_distinct() {
        let a = footstep(0).expect("step 0");
        let b = footstep(1).expect("step 1");
        assert_ne!(a.samples(), b.samples(), "variants differ");
        for clip in [
            a,
            b,
            swing().expect("swing"),
            hit(false).expect("hit"),
            fall().expect("fall"),
        ] {
            assert!(clip.samples().iter().all(|s| s.abs() <= 1.0 && s.is_finite()));
        }
    }
}
