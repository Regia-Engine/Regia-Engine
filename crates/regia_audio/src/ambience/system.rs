// SPDX-License-Identifier: MPL-2.0
//! The ambience systems: they read the weather, level the bed's voices and schedule the spots.
//! Both run in `PostUpdate` before `AudioSync`, only while there is something to do.

use std::collections::BTreeMap;
use std::time::Duration;

use regia_ecs::prelude::*;
use regia_scene::weather::{SceneWind, SurfaceWeather};
use regia_scene::{GlobalTransform, Transform};

use crate::audio::Audio;
use crate::bus::MUTE_DB;
use crate::sound::{LoopRegion, PlaybackParams};
use crate::sounds::SoundAssets;
use crate::spatial::{AudioEmitter, nearest_listener};

use super::bed::{AmbienceBed, AmbienceInputs};
use super::spots::{
    AmbientCall, AmbientSpot, AmbientSpotKind, SpotSchedule, call_position, next_interval, pick_variant,
};

/// Fade of a bed voice replaced by another sound, or stopped.
const BED_FADE: Duration = Duration::from_millis(500);

/// How fast a loop spot fades out when the weather silences it, and back in, dB per second.
const WEATHER_FADE_DB_PER_SEC: f32 = 30.0;

/// Lifetime of a call whose sound length is unknown, seconds.
const CALL_FALLBACK_SECS: f32 = 3.0;

/// The weather the systems read, as [`AmbienceInputs`].
fn inputs_of(wind: Option<&SceneWind>, surface: Option<&SurfaceWeather>) -> AmbienceInputs {
    AmbienceInputs::from_weather(wind, surface)
}

/// `PostUpdate` system (before `AudioSync`): steps the playing `.ambience.ron` files
/// ([`Audio::play_ambience`], ADR-2721) with the scene weather and player 0's listener, resolving
/// sound names through [`SoundAssets`]. Inert while none plays.
pub fn ambience_file_sync(
    time: Res<regia_app::Time>,
    mut audio: ResMut<Audio>,
    sounds: Option<Res<SoundAssets>>,
    wind: Option<Res<SceneWind>>,
    surface: Option<Res<SurfaceWeather>>,
) {
    if audio.ambience_files().is_empty() {
        return;
    }
    let inputs = inputs_of(wind.as_deref(), surface.as_deref());
    let listener = audio
        .listeners()
        .first()
        .map_or(regia_core::math::Vec3::ZERO, |(_, frame)| frame.position);
    let resolve = |name: &str| sounds.as_deref().and_then(|s| s.by_name(name)).cloned();
    audio.step_ambience_files(time.delta_secs(), &inputs, listener, &resolve);
}

/// `PostUpdate` system (before `AudioSync`, while an [`AmbienceBed`] exists): steps the bed with
/// the scene's [`SceneWind`] and [`SurfaceWeather`], starts each layer's loop once its sound is
/// registered, and sets every layer voice to its slewed level.
pub fn ambience_bed_sync(
    time: Res<regia_app::Time>,
    mut audio: ResMut<Audio>,
    mut bed: ResMut<AmbienceBed>,
    sounds: Option<Res<SoundAssets>>,
    wind: Option<Res<SceneWind>>,
    surface: Option<Res<SurfaceWeather>>,
) {
    let _span = tracing::trace_span!("ambience_bed_sync", layers = bed.layers.len()).entered();
    let dt = time.delta_secs();
    let inputs = bed.hear(wind.as_deref(), surface.as_deref());
    // A layer removed by the host leaves its state behind: stop that voice before the state goes.
    while bed.state.len() > bed.layers.len() {
        if let Some(id) = bed.state.pop().and_then(|s| s.voice) {
            audio.stop_voice(id, BED_FADE);
        }
    }
    bed.step(&inputs, dt);
    let bus = bed.bus.clone();
    for i in 0..bed.layers.len() {
        let wanted = bed.layers[i].sound.clone();
        // A voice that ended (or was stopped elsewhere) restarts.
        if let Some(id) = bed.state[i].voice
            && !audio.voice_alive(id)
        {
            bed.state[i].voice = None;
            bed.state[i].voice_sound = None;
        }
        if bed.state[i].voice_sound != wanted {
            if let Some(id) = bed.state[i].voice.take() {
                audio.stop_voice(id, BED_FADE);
            }
            bed.state[i].voice_sound = None;
            if let Some(asset) = wanted.as_deref().and_then(|n| sounds.as_deref()?.by_name(n)) {
                let params = PlaybackParams {
                    bus: Some(bus.clone()),
                    volume_db: MUTE_DB,
                    loop_region: Some(LoopRegion {
                        start_secs: 0.0,
                        end_secs: None,
                    }),
                    ..PlaybackParams::default()
                };
                match audio.start_asset(asset, params) {
                    Ok(id) => {
                        bed.state[i].voice = Some(id);
                        bed.state[i].voice_sound = wanted.clone();
                    }
                    Err(error) => {
                        tracing::warn!(target: "regia_audio", layer = %bed.layers[i].name, "ambience layer not started: {error}");
                    }
                }
            }
        }
        if let Some(id) = bed.state[i].voice {
            let db = bed.voice_db(i);
            audio.set_voice_volume_db(id, db, dt.clamp(1.0 / 240.0, 0.1));
        }
    }
}

/// `PostUpdate` system after [`ambience_bed_sync`]: remembers the bed's voices, and when the
/// [`AmbienceBed`] resource is removed, stops them (a removed bed falls silent). Idle without a bed.
pub(crate) fn ambience_bed_cleanup(
    audio: Option<ResMut<Audio>>,
    bed: Option<Res<AmbienceBed>>,
    mut playing: Local<Vec<crate::voices::SoundInstanceId>>,
) {
    match bed {
        Some(bed) => {
            playing.clear();
            playing.extend(bed.state.iter().filter_map(|s| s.voice));
        }
        None if !playing.is_empty() => {
            if let Some(mut audio) = audio {
                for id in playing.drain(..) {
                    audio.stop_voice(id, BED_FADE);
                }
            }
        }
        None => {}
    }
}

/// Stops every voice of `bed` (fading over 0.5 s) and forgets them. Removing the [`AmbienceBed`]
/// resource stops them too, one frame later (`AudioPlugin` keeps track of them).
pub fn stop_bed_voices(audio: &mut Audio, bed: &mut AmbienceBed) {
    for state in &mut bed.state {
        if let Some(id) = state.voice.take() {
            audio.stop_voice(id, BED_FADE);
        }
        state.voice_sound = None;
    }
}

/// The live schedules of the [`AmbientSpot`]s (one per spot entity).
#[derive(Resource, Debug, Default)]
pub struct AmbienceSpotState {
    /// Spot entity → its schedule.
    pub schedules: BTreeMap<Entity, SpotSchedule>,
    /// Calls spawned since start.
    pub calls_spawned: u64,
    /// Call entities live now.
    pub live_calls: usize,
}

/// `PostUpdate` system (before `AudioSync`, while a spot or a call exists): keeps each loop
/// spot's [`AudioEmitter`] in step with the weather, spawns the calls of scatter spots at their
/// random intervals and places (no immediate repeat of a variant), and despawns ended calls.
pub fn ambient_spot_sync(
    mut commands: Commands,
    time: Res<regia_app::Time>,
    (audio, sounds): (Res<Audio>, Option<Res<SoundAssets>>),
    (wind, surface): (Option<Res<SceneWind>>, Option<Res<SurfaceWeather>>),
    mut state: ResMut<AmbienceSpotState>,
    spots: Query<(
        Entity,
        &AmbientSpot,
        Option<&GlobalTransform>,
        Option<&AudioEmitter>,
    )>,
    mut calls: Query<(Entity, &mut AmbientCall)>,
) {
    let _span = tracing::trace_span!("ambient_spot_sync").entered();
    let dt = time.delta_secs().max(0.0);
    let inputs = inputs_of(wind.as_deref(), surface.as_deref());
    let listeners: Vec<_> = audio.listeners().iter().map(|(_, f)| *f).collect();

    // Ended calls.
    let mut live = 0;
    for (entity, mut call) in &mut calls {
        call.remaining_secs -= dt;
        if call.remaining_secs <= 0.0 {
            commands.entity(entity).despawn();
        } else {
            live += 1;
        }
    }

    let state = &mut *state;
    state.schedules.retain(|e, _| spots.contains(*e));
    for (entity, spot, global, current) in &spots {
        let centre = global.map(GlobalTransform::translation).unwrap_or_default();
        let schedule = state
            .schedules
            .entry(entity)
            .or_insert_with(|| SpotSchedule::new(spot, entity.to_bits()));
        let quiet = spot.quieted_by(&inputs);
        match spot.kind {
            AmbientSpotKind::Loop => {
                let Some(sound) = spot.sounds.first() else {
                    continue;
                };
                let target = if quiet { MUTE_DB } else { 0.0 };
                let step = WEATHER_FADE_DB_PER_SEC * dt;
                schedule.weather_db = if schedule.weather_db < target {
                    (schedule.weather_db + step).min(target)
                } else {
                    (schedule.weather_db - step).max(target)
                };
                let wanted = spot.emitter(sound, schedule.weather_db);
                let stale = current.is_none_or(|e| {
                    e.sound_name != wanted.sound_name
                        || (e.volume_db - wanted.volume_db).abs() > 0.1
                        || e.max_distance != wanted.max_distance
                        || e.min_distance != wanted.min_distance
                        || e.occlusion != wanted.occlusion
                        || e.bus != wanted.bus
                });
                if stale {
                    commands.entity(entity).insert(wanted);
                }
                schedule.looping = true;
            }
            AmbientSpotKind::Scatter { .. } => {
                // A loop spot turned into a scatter spot drops its loop emitter.
                if schedule.looping {
                    commands.entity(entity).remove::<AudioEmitter>();
                    schedule.looping = false;
                }
                schedule.next_call_in_secs -= dt;
                if schedule.next_call_in_secs > 0.0 {
                    continue;
                }
                schedule.next_call_in_secs = next_interval(spot, &mut schedule.rng).max(dt);
                let in_reach = nearest_listener(&listeners, centre)
                    .and_then(|i| listeners.get(i))
                    .is_some_and(|l| l.position.distance(centre) <= spot.reach());
                if quiet || !in_reach || spot.sounds.is_empty() {
                    schedule.skipped += 1;
                    continue;
                }
                let Some(variant) = pick_variant(&mut schedule.rng, spot.sounds.len(), schedule.last_variant)
                else {
                    continue;
                };
                schedule.last_variant = Some(variant);
                let position = call_position(spot, centre, &mut schedule.rng);
                let sound = &spot.sounds[variant];
                let length = sounds
                    .as_deref()
                    .and_then(|s| s.by_name(sound))
                    .map_or(CALL_FALLBACK_SECS, |a| a.duration_secs() as f32 + 0.25);
                commands.spawn((
                    Transform::from_translation(position),
                    GlobalTransform(Transform::from_translation(position).compute_affine()),
                    spot.emitter(sound, 0.0),
                    AmbientCall {
                        spot: entity,
                        remaining_secs: if length.is_finite() {
                            length
                        } else {
                            CALL_FALLBACK_SECS
                        },
                        variant,
                    },
                ));
                schedule.calls += 1;
                state.calls_spawned += 1;
                live += 1;
            }
        }
    }
    state.live_calls = live;
}
