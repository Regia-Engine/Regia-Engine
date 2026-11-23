// SPDX-License-Identifier: MPL-2.0
//! Agent methods for entity audio (`docs/specs/16-audio.md` § 3, specs/23 § 3):
//! `audio.list_emitters` reports every emitter (configuration, live voices, smoothed occlusion,
//! binaural state), the listeners, the registered sounds and the reverb-zone mix;
//! `audio.set_emitter` changes an emitter's fields (adding the component when the entity has
//! none), so an agent can place and tune sound without code and hear the result on the next
//! frame.
//!
//! Handlers are plain functions over `&mut World`; a world without the audio plugin returns
//! `UNSUPPORTED` with a hint naming it.

use regia_app::prelude::*;
use regia_audio::{Audio, AudioEmitter, AudioListener, CueAssets, SoundAssets};
use serde_json::{Map, Value, json};

use crate::protocol::{self, RpcError};

fn require_audio(world: &World) -> Result<(), RpcError> {
    if world.get_resource::<Audio>().is_none() {
        return Err(RpcError::new(
            protocol::UNSUPPORTED,
            "F0013",
            "no audio in this app",
            "add regia_audio::AudioPlugin (regia::DefaultPlugins includes it)",
        ));
    }
    Ok(())
}

fn v3(v: regia_core::math::Vec3) -> Value {
    json!([v.x, v.y, v.z])
}

fn emitter_json(world: &World, entity: Entity, emitter: &AudioEmitter) -> Value {
    let sound = match &emitter.sound {
        Some(handle) => world
            .get_resource::<SoundAssets>()
            .and_then(|s| s.name_of(handle).map(str::to_string))
            .unwrap_or_else(|| format!("<handle {}:{}>", handle.index, handle.generation)),
        None => emitter.sound_name.clone(),
    };
    // The cue an emitter plays (spec 16 § 3): named by handle (resolved through CueAssets) or by
    // cue_name; null when the emitter plays a sound instead.
    let cue = if emitter.has_cue() {
        match &emitter.cue {
            Some(handle) => world
                .get_resource::<CueAssets>()
                .and_then(|c| c.name_of(handle).map(str::to_string))
                .unwrap_or_else(|| format!("<handle {}:{}>", handle.index, handle.generation)),
            None => emitter.cue_name.clone(),
        }
        .into()
    } else {
        Value::Null
    };
    let status = world
        .get_resource::<Audio>()
        .and_then(|a| a.emitter_status().into_iter().find(|s| s.entity == entity));
    json!({
        "entity": entity.to_bits(),
        "name": crate::scene::entity_name(world, entity),
        "sound": sound,
        "cue": cue,
        "autoplay": emitter.autoplay,
        "looping": emitter.looping,
        "volume_db": emitter.volume_db,
        "min_distance": emitter.min_distance,
        "max_distance": emitter.max_distance,
        "fade_start": emitter.fade_start,
        "spatial_blend": emitter.spatial_blend,
        "cone_inner_degrees": emitter.cone_inner_degrees,
        "cone_outer_degrees": emitter.cone_outer_degrees,
        "cone_outer_gain": emitter.cone_outer_gain,
        "air_absorption": emitter.air_absorption,
        "doppler_factor": emitter.doppler_factor,
        "occlusion": emitter.occlusion,
        "bus": emitter.bus.clone().unwrap_or_else(|| "World".into()),
        "voices": status.as_ref().map_or(0, |s| s.voices),
        "occlusion_amount": status.as_ref().map_or(0.0, |s| s.occlusion),
        "rock_m": status.as_ref().map_or(0.0, |s| s.rock_m),
        "zone_scale": status.as_ref().map_or(1.0, |s| s.zone_scale),
        "routed": status.as_ref().is_some_and(|s| s.routed),
        "binaural": status.as_ref().is_some_and(|s| s.binaural),
        // The HRIR table rate the binaural voices use (the mixer's rate once its table exists,
        // ADR-7461); null when not binaural.
        "hrtf_rate": status.as_ref().and_then(|s| s.hrtf_rate),
        "position": status.as_ref().map(|s| v3(s.position)),
    })
}

/// The streaming underrun counters (`Audio::stream_stats`).
fn streams_json(s: regia_audio::StreamStats) -> Value {
    json!({
        "playing": s.streaming_voices,
        "starved": s.starved_voices,
        "underruns": s.underruns,
        "starved_secs": s.starved_secs,
    })
}

/// `audio.list_emitters`: emitters, listeners, registered sounds and the reverb mix.
pub fn list_emitters(world: &mut World) -> Result<Value, RpcError> {
    require_audio(world)?;
    let mut query = world.query::<(Entity, &AudioEmitter)>();
    let mut emitters: Vec<(Entity, AudioEmitter)> =
        query.iter(world).map(|(e, em)| (e, em.clone())).collect();
    emitters.sort_by_key(|(e, _)| *e);
    let emitters: Vec<Value> = emitters
        .iter()
        .map(|(entity, emitter)| emitter_json(world, *entity, emitter))
        .collect();
    let mut listener_query = world.query::<(Entity, &AudioListener)>();
    let hrtf: Vec<(Entity, bool)> = listener_query.iter(world).map(|(e, l)| (e, l.hrtf)).collect();
    let audio = world.resource::<Audio>();
    // Every binaural voice shares the one mixer, so a headphone listener's table rate is the
    // audio system's (ADR-7461); null for speakers or before a binaural voice runs.
    let table_rate = audio.hrtf_table_rate();
    let listeners: Vec<Value> = audio
        .listeners()
        .iter()
        .map(|(entity, frame)| {
            let on = entity
                .and_then(|e| hrtf.iter().find(|(h, _)| *h == e).map(|(_, on)| *on))
                .unwrap_or(false);
            json!({
                "entity": entity.map(Entity::to_bits),
                "position": v3(frame.position),
                "forward": v3(frame.forward),
                "velocity": v3(frame.velocity),
                "hrtf": on,
                "hrtf_rate": if on { table_rate } else { None },
            })
        })
        .collect();
    let reverb = audio.reverb_mix().map(|m| {
        json!({
            "weight": m.weight,
            "send_db": m.send_db,
            "feedback": m.feedback,
            "damping": m.damping,
            "stereo_width": m.stereo_width,
        })
    });
    let sounds: Vec<&str> = world
        .get_resource::<SoundAssets>()
        .map(|s| s.names().collect())
        .unwrap_or_default();
    let cues: Vec<&str> = world
        .get_resource::<CueAssets>()
        .map(|c| c.names().collect())
        .unwrap_or_default();
    Ok(json!({
        "emitters": emitters,
        "listeners": listeners,
        "sounds": sounds,
        "cues": cues,
        "loopgens": crate::audio::loopgens_json(world),
        "reverb": reverb,
        "real_voices": audio.real_voice_count(),
        "virtual_voices": audio.virtual_voice_count(),
        "output": {
            "suspended": audio.output_suspended(),
            "suspend_count": audio.suspend_count(),
            "resume_count": audio.resume_count(),
            "persistent_loops": audio.persistent_loop_count(),
            "persistent_patches": audio.persistent_patch_count(),
        },
        "rejected_values": regia_audio::rejected_value_count(),
        "streams": streams_json(audio.stream_stats()),
    }))
}

fn number(params: &Map<String, Value>, field: &str, range: (f32, f32)) -> Result<Option<f32>, RpcError> {
    let Some(value) = params.get(field) else {
        return Ok(None);
    };
    let n = value
        .as_f64()
        .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be a number")))? as f32;
    if !n.is_finite() || n < range.0 || n > range.1 {
        return Err(RpcError::invalid_params(format!(
            "`{field}` must be within {}..={}",
            range.0, range.1
        )));
    }
    Ok(Some(n))
}

fn boolean(params: &Map<String, Value>, field: &str) -> Result<Option<bool>, RpcError> {
    match params.get(field) {
        None => Ok(None),
        Some(v) => v
            .as_bool()
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be true or false"))),
    }
}

/// `audio.set_emitter`: changes the given fields of an entity's emitter (omitted fields keep
/// their value; an entity without an emitter gets a default one first).
pub fn set_emitter(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_audio(world)?;
    let entity = crate::scene::entity_from(params)?;
    if world.get_entity(entity).is_err() {
        return Err(RpcError::not_found(
            format!("entity {} does not exist", entity.to_bits()),
            "use scene.query_entities or audio.list_emitters",
        ));
    }
    let Some(fields) = params.as_object() else {
        return Err(RpcError::invalid_params("params must be an object"));
    };
    let existed = world.get::<AudioEmitter>(entity).is_some();
    let before = world.get::<AudioEmitter>(entity).cloned().unwrap_or_default();
    let mut after = before.clone();

    // A sound and a cue are mutually exclusive: naming both in one call is ambiguous, so it is
    // refused before anything changes ("" clears either and may accompany the other).
    let names = |field: &str| -> Result<bool, RpcError> {
        Ok(crate::params::opt_str(fields, field)?.is_some_and(|name| !name.is_empty()))
    };
    if names("sound")? && names("cue")? {
        return Err(RpcError::invalid_params(
            "`sound` and `cue` are mutually exclusive: name one (\"\" clears the other)",
        ));
    }
    if let Some(sound) = fields.get("sound") {
        let name = sound
            .as_str()
            .ok_or_else(|| RpcError::invalid_params("`sound` must be a sound name (\"\" clears it)"))?;
        if !name.is_empty() {
            let known: Vec<String> = world
                .get_resource::<SoundAssets>()
                .map(|s| s.names().map(str::to_string).collect())
                .unwrap_or_default();
            if !known.iter().any(|k| k == name) {
                return Err(RpcError::not_found(
                    format!("no sound named `{name}`"),
                    format!("registered sounds: {}", known.join(", ")),
                ));
            }
        }
        after.sound = None;
        after.sound_name = name.to_string();
        // A sound and a cue are mutually exclusive: setting one clears the other (spec 16 § 3).
        if !name.is_empty() {
            after.cue = None;
            after.cue_name.clear();
        }
    }
    if let Some(cue) = fields.get("cue") {
        let name = cue
            .as_str()
            .ok_or_else(|| RpcError::invalid_params("`cue` must be a cue name (\"\" clears it)"))?;
        if !name.is_empty() {
            let known: Vec<String> = world
                .get_resource::<CueAssets>()
                .map(|c| c.names().map(str::to_string).collect())
                .unwrap_or_default();
            if !known.iter().any(|k| k == name) {
                return Err(RpcError::not_found(
                    format!("no cue named `{name}`"),
                    format!("registered cues: {}", known.join(", ")),
                ));
            }
        }
        after.cue = None;
        after.cue_name = name.to_string();
        // A cue plays only when no sound is set: setting one clears the other (spec 16 § 3).
        if !name.is_empty() {
            after.sound = None;
            after.sound_name.clear();
        }
    }
    if let Some(bus) = fields.get("bus") {
        let bus = bus
            .as_str()
            .ok_or_else(|| RpcError::invalid_params("`bus` must be a bus name"))?;
        if world.resource::<Audio>().bus_volume(bus).is_err() {
            return Err(RpcError::invalid_params(format!("unknown bus `{bus}`")));
        }
        after.bus = (bus != "World").then(|| bus.to_string());
    }
    if let Some(v) = boolean(fields, "autoplay")? {
        after.autoplay = v;
    }
    if let Some(v) = boolean(fields, "looping")? {
        after.looping = v;
    }
    if let Some(v) = boolean(fields, "occlusion")? {
        after.occlusion = v;
    }
    if let Some(v) = boolean(fields, "air_absorption")? {
        after.air_absorption = v;
    }
    if let Some(v) = number(fields, "volume_db", (-100.0, 24.0))? {
        after.volume_db = v;
    }
    if let Some(v) = number(fields, "min_distance", (0.0, 100_000.0))? {
        after.min_distance = v;
    }
    if let Some(v) = number(fields, "max_distance", (0.001, 100_000.0))? {
        after.max_distance = v;
    }
    // `null` turns the fade off again; 1 and above would never fade.
    match fields.get("fade_start") {
        None => {}
        Some(Value::Null) => after.fade_start = None,
        Some(value) => {
            let fraction = value
                .as_f64()
                .filter(|v| (0.0..1.0).contains(v))
                .ok_or_else(|| RpcError::invalid_params("`fade_start` must be a number in 0..1, or null"))?;
            after.fade_start = Some(fraction as f32);
        }
    }
    if let Some(v) = number(fields, "spatial_blend", (0.0, 1.0))? {
        after.spatial_blend = v;
    }
    if let Some(v) = number(fields, "doppler_factor", (0.0, 10.0))? {
        after.doppler_factor = v;
    }
    if let Some(v) = number(fields, "cone_inner_degrees", (0.0, 360.0))? {
        after.cone_inner_degrees = v;
    }
    if let Some(v) = number(fields, "cone_outer_degrees", (0.0, 360.0))? {
        after.cone_outer_degrees = v;
    }
    if let Some(v) = number(fields, "cone_outer_gain", (0.0, 1.0))? {
        after.cone_outer_gain = v;
    }
    if after.max_distance < after.min_distance {
        return Err(RpcError::invalid_params(format!(
            "max_distance ({}) must be at least min_distance ({})",
            after.max_distance, after.min_distance
        )));
    }

    let before_json = existed.then(|| emitter_json(world, entity, &before));
    world.entity_mut(entity).insert(after.clone());
    Ok(json!({
        "created": !existed,
        "before": before_json,
        "after": emitter_json(world, entity, &after),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_audio::{AudioPlugin, AudioSettings, SoundAsset, SoundClip};
    use regia_scene::Transform;

    #[test]
    fn dispatched_output_state_tracks_persistent_loop_recovery() {
        fn read(app: &mut App) -> Value {
            crate::dispatch::call_in_process(app.world_mut(), "audio.list_emitters", json!({}))
                .expect("synchronous read")
                .expect("audio state")
        }
        let mut app = app();
        app.insert_resource(crate::safety::AgentPermissions::default());
        let asset = SoundAsset::from_clip(&SoundClip::new(vec![0.1; 4_800], 48_000).expect("clip"));
        let id = app
            .world_mut()
            .resource_mut::<Audio>()
            .start_persistent_loop(&asset, regia_audio::PlaybackParams::default())
            .expect("persistent loop");
        let before = read(&mut app);
        assert_eq!(before["output"]["persistent_loops"], 1);
        assert_eq!(before["real_voices"], 1);
        app.world_mut().resource_mut::<Audio>().suspend_output();
        let suspended = read(&mut app);
        assert_eq!(suspended["output"]["suspended"], true);
        assert_eq!(suspended["output"]["suspend_count"], 1);
        assert_eq!(suspended["output"]["persistent_loops"], 1);
        assert_eq!(suspended["real_voices"], 0);
        assert!(app.world_mut().resource_mut::<Audio>().resume_output());
        let resumed = read(&mut app);
        assert_eq!(resumed["output"]["suspended"], false);
        assert_eq!(resumed["output"]["resume_count"], 1);
        assert_eq!(resumed["real_voices"], 1);
        assert!(app.world().resource::<Audio>().voice_alive(id));
        let rendered = crate::dispatch::call_in_process(
            app.world_mut(),
            "audio.render_check",
            json!({ "seconds": 0.1 }),
        )
        .expect("synchronous render")
        .expect("offline render");
        assert!(
            rendered["peak_db"].as_f64().is_some_and(|db| db > -40.0),
            "recovered output is audible"
        );
    }

    #[test]
    fn the_fade_start_is_set_cleared_and_range_checked() {
        let mut app = app();
        let entity = app.world_mut().spawn(Transform::IDENTITY).id();
        let bits = entity.to_bits();
        let out = set_emitter(app.world_mut(), &json!({ "entity": bits, "fade_start": 0.75 })).expect("sets");
        assert_eq!(out["after"]["fade_start"], 0.75);
        assert_eq!(
            app.world().get::<AudioEmitter>(entity).and_then(|e| e.fade_start),
            Some(0.75)
        );
        for bad in [json!(1.0), json!(-0.1), json!("soon")] {
            assert!(set_emitter(app.world_mut(), &json!({ "entity": bits, "fade_start": bad })).is_err());
        }
        let out =
            set_emitter(app.world_mut(), &json!({ "entity": bits, "fade_start": null })).expect("clears");
        assert_eq!(out["after"]["fade_start"], Value::Null);
    }

    /// AU-14 (ADR-7461), through the dispatcher: on a 96 kHz mixer a headphone listener's
    /// binaural emitter reports the 96 kHz HRIR table, on both the emitter and the listener.
    #[test]
    fn list_emitters_reports_the_hrtf_table_rate_of_a_96k_mixer() {
        let mut app = App::new();
        app.insert_resource(AudioSettings {
            headless: true,
            sample_rate: 96_000,
            ..Default::default()
        });
        app.add_plugins((regia_scene::ScenePlugin, AudioPlugin));
        let clip = SoundClip::new(vec![0.1; 9_600], 96_000).expect("clip");
        app.world_mut()
            .resource_mut::<SoundAssets>()
            .add("hum", SoundAsset::from_clip(&clip));
        app.world_mut()
            .spawn((AudioListener { hrtf: true }, Transform::IDENTITY));
        // A second, speaker listener far away: the emitter is heard by the nearest (headphones).
        app.world_mut().spawn((
            AudioListener { hrtf: false },
            Transform::from_translation(regia_core::math::Vec3::X * 500.0),
        ));
        let entity = app
            .world_mut()
            .spawn(Transform::from_translation(regia_core::math::Vec3::NEG_X * 0.5))
            .id();
        crate::dispatch::call_in_process(
            app.world_mut(),
            "audio.set_emitter",
            json!({ "entity": entity.to_bits(), "sound": "hum", "occlusion": false }),
        )
        .expect("immediate answer")
        .expect("set");
        for _ in 0..3 {
            app.update();
        }
        let list = crate::dispatch::call_in_process(app.world_mut(), "audio.list_emitters", json!({}))
            .expect("immediate answer")
            .expect("list");
        assert_eq!(list["emitters"][0]["binaural"], json!(true), "{list}");
        assert_eq!(list["emitters"][0]["hrtf_rate"], json!(96_000), "{list}");
        let listeners = list["listeners"].as_array().expect("listeners");
        let rate_of = |hrtf: bool| {
            listeners
                .iter()
                .find(|l| l["hrtf"] == json!(hrtf))
                .map(|l| l["hrtf_rate"].clone())
        };
        assert_eq!(rate_of(true), Some(json!(96_000)), "{list}");
        assert_eq!(
            rate_of(false),
            Some(Value::Null),
            "speakers have no table: {list}"
        );
    }

    fn app() -> App {
        let mut app = App::new();
        app.insert_resource(AudioSettings {
            headless: true,
            ..Default::default()
        });
        app.add_plugins(AudioPlugin);
        let clip = SoundClip::new(vec![0.1; 4_800], 48_000).expect("clip");
        app.world_mut()
            .resource_mut::<SoundAssets>()
            .add("hum", SoundAsset::from_clip(&clip));
        app.world_mut()
            .resource_mut::<regia_audio::CueAssets>()
            .add("steps", {
                let mut cue = regia_audio::SoundCue::new(
                    "steps",
                    vec![regia_audio::CueEntry {
                        asset: SoundAsset::from_clip(&clip),
                        weight: 1.0,
                    }],
                );
                cue.max_concurrent = None;
                cue
            });
        app
    }

    #[test]
    fn set_emitter_accepts_and_reports_a_cue() {
        let mut app = app();
        let entity = app
            .world_mut()
            .spawn(Transform::from_translation(regia_core::math::Vec3::NEG_Z * 3.0))
            .id();
        let result = set_emitter(
            app.world_mut(),
            &json!({ "entity": entity.to_bits(), "cue": "steps" }),
        )
        .expect("set");
        assert_eq!(result["created"], json!(true));
        assert_eq!(result["after"]["cue"], json!("steps"));
        assert_eq!(result["after"]["sound"], json!(""), "a cue clears the sound");
        app.update();
        let list = list_emitters(app.world_mut()).expect("list");
        assert_eq!(list["emitters"][0]["cue"], json!("steps"));
        assert_eq!(list["cues"], json!(["steps"]));
        assert_eq!(list["emitters"][0]["voices"], json!(1), "the cue autoplayed");
        // A sound and a cue in one call are refused, and nothing changes (rev-kimi-10 finding 9).
        let err = set_emitter(
            app.world_mut(),
            &json!({ "entity": entity.to_bits(), "sound": "hum", "cue": "steps" }),
        )
        .expect_err("both named");
        assert_eq!(err.code, protocol::INVALID_PARAMS);
        let unchanged = list_emitters(app.world_mut()).expect("list");
        assert_eq!(
            unchanged["emitters"][0]["cue"],
            json!("steps"),
            "the refused call changed nothing"
        );
        assert_eq!(unchanged["emitters"][0]["sound"], json!(""));
        // Setting a sound clears the cue.
        let result = set_emitter(
            app.world_mut(),
            &json!({ "entity": entity.to_bits(), "sound": "hum" }),
        )
        .expect("set sound");
        assert_eq!(result["after"]["sound"], json!("hum"));
        assert_eq!(result["after"]["cue"], Value::Null);
        // An unknown cue is a not-found.
        let err = set_emitter(
            app.world_mut(),
            &json!({ "entity": entity.to_bits(), "cue": "nope" }),
        )
        .expect_err("unknown cue");
        assert_eq!(err.code, protocol::NOT_FOUND);
    }

    #[test]
    fn set_emitter_adds_and_plays_and_list_reports_it() {
        let mut app = app();
        let entity = app
            .world_mut()
            .spawn(Transform::from_translation(regia_core::math::Vec3::NEG_Z * 3.0))
            .id();
        let result = set_emitter(
            app.world_mut(),
            &json!({ "entity": entity.to_bits(), "sound": "hum", "volume_db": -6.0, "occlusion": false }),
        )
        .expect("set");
        assert_eq!(result["created"], json!(true));
        assert_eq!(result["after"]["sound"], json!("hum"));
        app.update();
        let list = list_emitters(app.world_mut()).expect("list");
        let emitters = list["emitters"].as_array().expect("array");
        assert_eq!(emitters.len(), 1);
        assert_eq!(emitters[0]["voices"], json!(1), "autoplay started it: {list}");
        assert_eq!(emitters[0]["volume_db"], json!(-6.0));
        assert_eq!(list["sounds"], json!(["hum"]));
        assert_eq!(list["listeners"].as_array().map(Vec::len), Some(1));
        // Process-wide and monotonic: other tests may have added to it, never taken away.
        assert!(list["rejected_values"].as_u64().is_some(), "{list}");
        assert_eq!(list["streams"]["underruns"], json!(0), "no stream played: {list}");
    }

    #[test]
    fn set_emitter_validates() {
        let mut app = app();
        let entity = app.world_mut().spawn_empty().id();
        let bits = entity.to_bits();
        let bad =
            |app: &mut App, params: Value| set_emitter(app.world_mut(), &params).expect_err("rejected").code;
        assert_eq!(
            bad(&mut app, json!({ "entity": bits, "sound": "nope" })),
            protocol::NOT_FOUND
        );
        assert_eq!(
            bad(&mut app, json!({ "entity": bits, "spatial_blend": 2.0 })),
            protocol::INVALID_PARAMS
        );
        assert_eq!(
            bad(
                &mut app,
                json!({ "entity": bits, "min_distance": 10.0, "max_distance": 5.0 })
            ),
            protocol::INVALID_PARAMS
        );
        assert_eq!(
            bad(&mut app, json!({ "entity": bits, "bus": "Nope" })),
            protocol::INVALID_PARAMS
        );
        // Nothing was inserted by the failed calls.
        assert!(app.world().get::<AudioEmitter>(entity).is_none());
    }

    #[test]
    fn without_audio_the_methods_are_unsupported() {
        let mut app = App::new();
        let err = list_emitters(app.world_mut()).expect_err("no audio");
        assert_eq!(err.code, protocol::UNSUPPORTED);
    }
}
