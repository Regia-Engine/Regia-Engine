// SPDX-License-Identifier: MPL-2.0
//! Ambience on the agent protocol (`book/agents/audio-ambience.md`, ADR-2442): the weather-driven
//! bed (`audio.ambience`, `audio.set_ambience`), point ambiences (`audio.ambient_spots`,
//! `audio.set_ambient_spot`, `audio.probe_ambience`) and surface footsteps (`audio.footsteps`,
//! `audio.set_footsteps`, `audio.footstep`).
//!
//! `audio.ambience` also reports the renderer's foliage wind next to the bed's wind level, so an
//! agent can check that what is heard matches what is drawn (`coherence.agree`).

use regia_app::prelude::*;
use regia_audio::{
    AmbienceBed, AmbienceInputs, AmbienceSpotState, AmbientSpot, AmbientSpotKind, Audio, AudioOcclusion,
    BedDrive, BedLayer, FootstepBank, FootstepEvent, FootstepPlay, FootstepSet, MUTE_DB, RolloffCurve,
    SoundAssets, TEST_TONE_PREFIX, spot_hearing, stop_bed_voices, test_tone_loopgens,
};
use regia_core::math::Vec3;
use regia_framework::footsteps::FootstepTracker;
use regia_scene::weather::{SceneWind, SurfaceWeather};
use regia_scene::{GlobalTransform, Transform};
use serde_json::{Map, Value, json};

use crate::protocol::{self, RpcError};

/// Below this wind (m/s) the renderer's sway stage leaves foliage still (`FoliageSway::offset`).
const FOLIAGE_STILL_M_PER_S: f32 = 1.0e-4;

/// The bed's wind layers count as silent at or below this level, dB.
const WIND_SILENT_DB: f32 = -60.0;

/// From this foliage wind (m/s) the sway is plainly visible (a 2 m shrub's top moves about 6 cm), so
/// silent wind layers no longer match the picture.
const FOLIAGE_MOVING_M_PER_S: f32 = 2.0;

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

fn ambience_error(code: &'static str, message: impl Into<String>, hint: &str) -> RpcError {
    RpcError::new(protocol::INVALID_PARAMS, code, message, hint)
}

fn obj(params: &Value) -> Result<&Map<String, Value>, RpcError> {
    static EMPTY: std::sync::OnceLock<Map<String, Value>> = std::sync::OnceLock::new();
    match params {
        Value::Null => Ok(EMPTY.get_or_init(Map::new)),
        Value::Object(map) => Ok(map),
        _ => Err(RpcError::invalid_params("params must be an object")),
    }
}

fn num(params: &Map<String, Value>, field: &str, range: (f32, f32)) -> Result<Option<f32>, RpcError> {
    let Some(value) = params.get(field) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
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

fn string(params: &Map<String, Value>, field: &str) -> Result<Option<String>, RpcError> {
    match params.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(RpcError::invalid_params(format!("`{field}` must be a string"))),
    }
}

fn boolean(params: &Map<String, Value>, field: &str) -> Result<Option<bool>, RpcError> {
    match params.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_bool()
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be true or false"))),
    }
}

fn vec3(value: &Value, field: &str) -> Result<Vec3, RpcError> {
    let bad = || RpcError::invalid_params(format!("`{field}` must be [x, y, z] (finite numbers)"));
    let a = value.as_array().filter(|a| a.len() == 3).ok_or_else(bad)?;
    let mut out = [0.0f32; 3];
    for (slot, v) in out.iter_mut().zip(a) {
        *slot = v.as_f64().filter(|n| n.is_finite()).ok_or_else(bad)? as f32;
    }
    Ok(Vec3::from_array(out))
}

fn strings(value: &Value, field: &str) -> Result<Vec<String>, RpcError> {
    let bad = || RpcError::invalid_params(format!("`{field}` must be an array of strings"));
    value
        .as_array()
        .ok_or_else(bad)?
        .iter()
        .map(|v| v.as_str().map(str::to_string).ok_or_else(bad))
        .collect()
}

fn db(x: f32) -> Value {
    if x <= MUTE_DB { json!(MUTE_DB) } else { json!(x) }
}

fn inputs_json(inputs: &AmbienceInputs) -> Value {
    json!({
        "wind_m_per_s": inputs.wind_m_per_s,
        "mean_wind_m_per_s": inputs.mean_wind_m_per_s,
        "gust_m_per_s": inputs.gust_m_per_s(),
        "rain_mm_per_h": inputs.rain_mm_per_h,
        "snow_mm_per_h": inputs.snow_mm_per_h,
        "wetness": inputs.wetness,
    })
}

fn bed_json(bed: &AmbienceBed, sounds: Option<&SoundAssets>) -> Value {
    let layers: Vec<Value> = bed
        .layers
        .iter()
        .enumerate()
        .map(|(i, layer)| {
            let state = bed.state.get(i).cloned().unwrap_or_default();
            let registered = layer
                .sound
                .as_deref()
                .is_some_and(|n| sounds.is_some_and(|s| s.by_name(n).is_some()));
            json!({
                "name": layer.name,
                "sound": layer.sound,
                "drive": layer.drive.name(),
                "curve": layer.curve,
                "drive_value": state.drive_value,
                "target_db": db(state.target_db),
                "level_db": db(state.level_db()),
                "playing": state.voice.is_some(),
                "missing_sound": layer.sound.is_some() && !registered,
            })
        })
        .collect();
    json!({
        "calm_m_per_s": bed.calm_m_per_s,
        "attack_secs": bed.attack_secs,
        "release_secs": bed.release_secs,
        "bus": bed.bus,
        "volume_db": bed.volume_db,
        "wind_override_m_per_s": bed.wind_override,
        "inputs": inputs_json(&bed.inputs),
        "layers": layers,
        "wind_level_db": db(bed.wind_level_db()),
    })
}

/// `audio.ambience`: `{ bed, weather, foliage, coherence, spots }`.
pub(crate) fn get(world: &mut World) -> Result<Value, RpcError> {
    require_audio(world)?;
    let weather = AmbienceInputs::from_weather(
        world.get_resource::<SceneWind>(),
        world.get_resource::<SurfaceWeather>(),
    );
    let bed = world
        .get_resource::<AmbienceBed>()
        .map(|b| bed_json(b, world.get_resource::<SoundAssets>()));
    let foliage = world.get_resource::<regia_render::RuntimeRenderer>().map(|r| {
        let wind = r.0.foliage_wind();
        let speed = wind.map_or(0.0, |[x, z]| x.hypot(z));
        json!({ "wind_m_per_s": wind, "speed_m_per_s": speed, "still": speed <= FOLIAGE_STILL_M_PER_S })
    });
    let coherence = world.get_resource::<AmbienceBed>().map(|b| {
        // Without a renderer the scene wind is what the foliage would read (spec 99 § 3.3).
        let foliage_speed = foliage
            .as_ref()
            .and_then(|f| f["speed_m_per_s"].as_f64())
            .map(|v| v as f32)
            .unwrap_or_else(|| {
                world
                    .get_resource::<SceneWind>()
                    .and_then(SceneWind::active)
                    .map_or(0.0, |[x, z]| x.hypot(z))
            });
        let foliage_still = foliage_speed <= FOLIAGE_STILL_M_PER_S;
        let level = b.wind_level_db();
        let wind_silent = level <= WIND_SILENT_DB;
        // Still foliage with wind heard, or plainly swaying foliage with none, does not match.
        let agree =
            !(foliage_still && !wind_silent) && !(foliage_speed >= FOLIAGE_MOVING_M_PER_S && wind_silent);
        json!({
            "foliage_speed_m_per_s": foliage_speed,
            "foliage_still": foliage_still,
            "wind_silent": wind_silent,
            "agree": agree,
        })
    });
    let spots = world.get_resource::<AmbienceSpotState>().map(|s| {
        json!({ "count": s.schedules.len(), "calls_spawned": s.calls_spawned, "live_calls": s.live_calls })
    });
    Ok(json!({
        "bed": bed,
        "weather": inputs_json(&weather),
        "foliage": foliage,
        "coherence": coherence,
        "spots": spots,
    }))
}

fn parse_curve(value: &Value) -> Result<Vec<(f32, f32)>, RpcError> {
    let bad = || RpcError::invalid_params("`curve` must be [[value, dB], ...] with 1 to 32 finite points");
    let points = value
        .as_array()
        .filter(|a| !a.is_empty() && a.len() <= 32)
        .ok_or_else(bad)?;
    points
        .iter()
        .map(|p| {
            let pair = p.as_array().filter(|a| a.len() == 2).ok_or_else(bad)?;
            let x = pair[0].as_f64().filter(|n| n.is_finite()).ok_or_else(bad)? as f32;
            let y = pair[1].as_f64().filter(|n| n.is_finite()).ok_or_else(bad)? as f32;
            Ok((x, y.clamp(MUTE_DB, 12.0)))
        })
        .collect()
}

/// Applies the `layers` edits; returns the voices of removed layers (the caller stops them).
fn apply_layers(
    bed: &mut AmbienceBed,
    layers: &Value,
) -> Result<Vec<regia_audio::SoundInstanceId>, RpcError> {
    let mut removed = Vec::new();
    let list = layers
        .as_array()
        .ok_or_else(|| RpcError::invalid_params("`layers` must be an array of layer objects"))?;
    for entry in list {
        let o = entry
            .as_object()
            .ok_or_else(|| RpcError::invalid_params("each layer must be an object with a `name`"))?;
        let name = string(o, "name")?.ok_or_else(|| RpcError::invalid_params("each layer needs a `name`"))?;
        if boolean(o, "remove")? == Some(true) {
            if let Some(i) = bed.layer_index(&name) {
                bed.layers.remove(i);
                if i < bed.state.len() {
                    removed.extend(bed.state.remove(i).voice);
                }
            }
            continue;
        }
        let drive = match string(o, "drive")? {
            Some(d) => Some(BedDrive::from_name(&d).ok_or_else(|| {
                ambience_error(
                    "ambience_unknown_drive",
                    format!("unknown drive `{d}`"),
                    "use constant, wind, gust, rustle, rain, drips or snow",
                )
            })?),
            None => None,
        };
        let curve = o.get("curve").map(parse_curve).transpose()?;
        let index = match bed.layer_index(&name) {
            Some(i) => i,
            None => {
                let (Some(drive), Some(curve)) = (drive, curve.clone()) else {
                    return Err(RpcError::invalid_params(format!(
                        "a new layer `{name}` needs `drive` and `curve`"
                    )));
                };
                bed.layers.push(BedLayer::new(name.clone(), drive, curve));
                bed.state.push(Default::default());
                bed.layers.len() - 1
            }
        };
        let layer = &mut bed.layers[index];
        if let Some(drive) = drive {
            layer.drive = drive;
        }
        if let Some(curve) = curve {
            layer.curve = curve;
        }
        if o.contains_key("sound") {
            layer.sound = string(o, "sound")?;
        }
    }
    Ok(removed)
}

/// `audio.set_ambience { enabled?, preset?, calm_m_per_s?, attack_secs?, release_secs?, volume_db?,
/// bus?, layers? }` → `{ before, after }`.
pub(crate) fn set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_audio(world)?;
    let p = obj(params)?;
    let sounds = world.get_resource::<SoundAssets>();
    let before = world.get_resource::<AmbienceBed>().map(|b| bed_json(b, sounds));
    if boolean(p, "enabled")? == Some(false) {
        if let Some(mut bed) = world.remove_resource::<AmbienceBed>() {
            let mut audio = world.resource_mut::<Audio>();
            stop_bed_voices(&mut audio, &mut bed);
        }
        return Ok(json!({ "before": before, "after": null }));
    }
    let mut bed = match string(p, "preset")?.as_deref() {
        Some("forest") => {
            let mut fresh = AmbienceBed::forest();
            if let Some(mut old) = world.remove_resource::<AmbienceBed>() {
                stop_bed_voices(&mut world.resource_mut::<Audio>(), &mut old);
            }
            fresh.state.iter_mut().for_each(|s| *s = Default::default());
            fresh
        }
        Some("empty") => {
            if let Some(mut old) = world.remove_resource::<AmbienceBed>() {
                stop_bed_voices(&mut world.resource_mut::<Audio>(), &mut old);
            }
            AmbienceBed::with_layers(Vec::new())
        }
        Some(other) => {
            return Err(ambience_error(
                "ambience_unknown_preset",
                format!("unknown preset `{other}`"),
                "use forest or empty",
            ));
        }
        None => world
            .get_resource::<AmbienceBed>()
            .cloned()
            .unwrap_or_else(AmbienceBed::forest),
    };
    if let Some(v) = num(p, "calm_m_per_s", (0.0, 40.0))? {
        bed.calm_m_per_s = v;
    }
    if let Some(v) = num(p, "attack_secs", (0.0, 30.0))? {
        bed.attack_secs = v;
    }
    if let Some(v) = num(p, "release_secs", (0.0, 60.0))? {
        bed.release_secs = v;
    }
    if let Some(v) = num(p, "volume_db", (-60.0, 12.0))? {
        bed.volume_db = v;
    }
    if let Some(bus) = string(p, "bus")? {
        bed.bus = bus;
    }
    if let Some(raw) = p.get("wind_override_m_per_s") {
        bed.wind_override = if raw.is_null() {
            None
        } else {
            let pair = raw
                .as_array()
                .filter(|a| a.len() == 2)
                .and_then(|a| Some([a[0].as_f64()? as f32, a[1].as_f64()? as f32]))
                .filter(|w| w.iter().all(|v| v.is_finite() && v.abs() <= 100.0))
                .ok_or_else(|| {
                    RpcError::invalid_params(
                        "`wind_override_m_per_s` must be [x, z] (m/s, |v| <= 100) or null",
                    )
                })?;
            Some(pair)
        };
    }
    let removed = match p.get("layers") {
        Some(layers) => apply_layers(&mut bed, layers)?,
        None => Vec::new(),
    };
    if boolean(p, "test_tones")? == Some(true) {
        // Generated placeholder loops (shaped noise), for layers without a sound of their own.
        let mut sounds = world.get_resource_mut::<SoundAssets>().ok_or_else(|| {
            RpcError::not_found("no SoundAssets in this app", "add regia_audio::AudioPlugin")
        })?;
        for (layer, text) in test_tone_loopgens() {
            let name = format!("{TEST_TONE_PREFIX}{layer}");
            if sounds.by_name(&name).is_none() {
                sounds
                    .add_loopgen(name.clone(), &text)
                    .map_err(|e| RpcError::internal(format!("test tone `{layer}` did not render: {e}")))?;
            }
            if let Some(i) = bed.layer_index(layer)
                && bed.layers[i].sound.is_none()
            {
                bed.layers[i].sound = Some(name);
            }
        }
    }
    let after = bed_json(&bed, world.get_resource::<SoundAssets>());
    world.insert_resource(bed);
    let mut audio = world.resource_mut::<Audio>();
    for id in removed {
        audio.stop_voice(id, std::time::Duration::from_millis(500));
    }
    Ok(json!({ "before": before, "after": after }))
}

fn rolloff_json(curve: &RolloffCurve) -> Value {
    match curve {
        RolloffCurve::Inverse => json!({ "kind": "inverse" }),
        RolloffCurve::Linear => json!({ "kind": "linear" }),
        RolloffCurve::Log { db_per_doubling } => json!({ "kind": "log", "db_per_doubling": db_per_doubling }),
        RolloffCurve::Custom(points) => json!({ "kind": "custom", "points": points }),
    }
}

fn spot_json(spot: &AmbientSpot) -> Value {
    let kind = match spot.kind {
        AmbientSpotKind::Loop => json!({ "kind": "loop" }),
        AmbientSpotKind::Scatter {
            min_interval_secs,
            max_interval_secs,
            radius_m,
            height_m,
        } => json!({
            "kind": "scatter",
            "interval_secs": [min_interval_secs, max_interval_secs],
            "radius_m": radius_m,
            "height_m": height_m,
        }),
    };
    json!({
        "sounds": spot.sounds,
        "kind": kind,
        "volume_db": spot.volume_db,
        "min_distance": spot.min_distance,
        "max_distance": spot.max_distance,
        "rolloff": rolloff_json(&spot.rolloff),
        "occlusion": spot.occlusion,
        "quiet_above_wind_m_per_s": spot.quiet_above_wind_m_per_s,
        "quiet_above_rain_mm_per_h": spot.quiet_above_rain_mm_per_h,
        "bus": spot.bus,
        "seed": spot.seed,
    })
}

fn hearing_json(h: &regia_audio::SpotHearing) -> Value {
    json!({
        "distance": if h.distance.is_finite() { json!(h.distance) } else { Value::Null },
        "gain": h.gain,
        "gain_db": db(h.gain_db),
        "cutoff_hz": h.cutoff_hz,
        "occlusion": h.occlusion,
        "quieted": h.quieted,
        "audible": h.audible,
    })
}

/// `audio.ambient_spots`: every spot with its schedule and what player 0 hears of it now.
pub(crate) fn list_spots(world: &mut World) -> Result<Value, RpcError> {
    require_audio(world)?;
    let inputs = AmbienceInputs::from_weather(
        world.get_resource::<SceneWind>(),
        world.get_resource::<SurfaceWeather>(),
    );
    let mut query = world.query::<(Entity, &AmbientSpot, Option<&GlobalTransform>)>();
    let mut rows: Vec<(Entity, AmbientSpot, Vec3)> = query
        .iter(world)
        .map(|(e, s, g)| {
            (
                e,
                s.clone(),
                g.map(GlobalTransform::translation).unwrap_or_default(),
            )
        })
        .collect();
    rows.sort_by_key(|(e, _, _)| *e);
    let audio = world.resource::<Audio>();
    let listener = audio
        .listeners()
        .first()
        .map(|(_, f)| f.position)
        .unwrap_or_default();
    let medium = audio.medium().clone();
    let occlusion = world.get_resource::<AudioOcclusion>();
    let tuning = occlusion.map(|o| o.tuning).unwrap_or_default();
    let status = audio.emitter_status();
    let state = world.get_resource::<AmbienceSpotState>();
    let spots: Vec<Value> = rows
        .iter()
        .map(|(entity, spot, position)| {
            let live_occlusion = status
                .iter()
                .find(|s| s.entity == *entity)
                .map(|s| s.occlusion)
                .or_else(|| occlusion.map(|o| o.reported(*entity)))
                .unwrap_or(0.0);
            let hearing = spot_hearing(
                spot,
                listener,
                *position,
                live_occlusion,
                &medium,
                &tuning,
                &inputs,
            );
            let schedule = state.and_then(|s| s.schedules.get(entity)).map(|s| {
                json!({
                    "calls": s.calls,
                    "skipped": s.skipped,
                    "next_call_in_secs": s.next_call_in_secs,
                    "last_variant": s.last_variant,
                    "looping": s.looping,
                    "weather_db": db(s.weather_db),
                })
            });
            let mut row = spot_json(spot);
            row["entity"] = json!(entity.to_bits());
            row["position"] = json!([position.x, position.y, position.z]);
            row["hearing"] = hearing_json(&hearing);
            row["schedule"] = schedule.unwrap_or(Value::Null);
            row
        })
        .collect();
    Ok(json!({ "spots": spots, "count": rows.len(), "listener": [listener.x, listener.y, listener.z] }))
}

fn entity_of(world: &World, bits: &Value) -> Result<Entity, RpcError> {
    let bits = bits
        .as_u64()
        .ok_or_else(|| RpcError::invalid_params("`entity` must be an entity id (integer)"))?;
    let entity = Entity::try_from_bits(bits)
        .ok_or_else(|| RpcError::not_found(format!("no entity {bits}"), "use audio.ambient_spots"))?;
    if world.get_entity(entity).is_err() {
        return Err(RpcError::not_found(
            format!("no entity {bits}"),
            "use audio.ambient_spots",
        ));
    }
    Ok(entity)
}

fn apply_spot(spot: &mut AmbientSpot, p: &Map<String, Value>) -> Result<(), RpcError> {
    if let Some(sounds) = p.get("sounds") {
        spot.sounds = strings(sounds, "sounds")?;
    }
    if let Some(kind) = string(p, "kind")? {
        spot.kind = match kind.as_str() {
            "loop" => AmbientSpotKind::Loop,
            "scatter" => match spot.kind {
                AmbientSpotKind::Scatter { .. } => spot.kind,
                AmbientSpotKind::Loop => AmbientSpot::birds(Vec::new()).kind,
            },
            other => {
                return Err(ambience_error(
                    "ambience_unknown_kind",
                    format!("unknown spot kind `{other}`"),
                    "use loop or scatter",
                ));
            }
        };
    }
    if let Some(v) = num(p, "volume_db", (-80.0, 12.0))? {
        spot.volume_db = v;
    }
    if let Some(v) = num(p, "min_distance", (0.0, 100_000.0))? {
        spot.min_distance = v;
    }
    if let Some(v) = num(p, "max_distance", (0.001, 100_000.0))? {
        spot.max_distance = v;
    }
    if let Some(v) = boolean(p, "occlusion")? {
        spot.occlusion = v;
    }
    if p.contains_key("quiet_above_wind_m_per_s") {
        spot.quiet_above_wind_m_per_s = num(p, "quiet_above_wind_m_per_s", (0.0, 100.0))?;
    }
    if p.contains_key("quiet_above_rain_mm_per_h") {
        spot.quiet_above_rain_mm_per_h = num(p, "quiet_above_rain_mm_per_h", (0.0, 500.0))?;
    }
    if let Some(bus) = string(p, "bus")? {
        spot.bus = bus;
    }
    if let Some(seed) = p.get("seed") {
        spot.seed = seed
            .as_u64()
            .ok_or_else(|| RpcError::invalid_params("`seed` must be a non-negative integer"))?;
    }
    if let AmbientSpotKind::Scatter {
        min_interval_secs,
        max_interval_secs,
        radius_m,
        height_m,
    } = &mut spot.kind
    {
        if let Some(range) = p.get("interval_secs") {
            let bad =
                || RpcError::invalid_params("`interval_secs` must be [min, max] seconds, 0.05 <= min <= max");
            let a = range.as_array().filter(|a| a.len() == 2).ok_or_else(bad)?;
            let lo = a[0].as_f64().ok_or_else(bad)? as f32;
            let hi = a[1].as_f64().ok_or_else(bad)? as f32;
            if !(lo.is_finite() && hi.is_finite() && lo >= 0.05 && hi >= lo && hi <= 3600.0) {
                return Err(bad());
            }
            *min_interval_secs = lo;
            *max_interval_secs = hi;
        }
        if let Some(v) = num(p, "radius_m", (0.0, 10_000.0))? {
            *radius_m = v;
        }
        if let Some(v) = num(p, "height_m", (0.0, 1_000.0))? {
            *height_m = v;
        }
    }
    if spot.min_distance > spot.max_distance {
        return Err(RpcError::invalid_params(
            "`min_distance` must not exceed `max_distance`",
        ));
    }
    Ok(())
}

/// `audio.set_ambient_spot { entity? | position?, preset?, remove?, ... }` → `{ entity, created, spot }`.
pub(crate) fn set_spot(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_audio(world)?;
    let p = obj(params)?;
    let preset = string(p, "preset")?;
    let base = match preset.as_deref() {
        None => None,
        Some("water") => Some(AmbientSpot::water(String::new())),
        Some("birds") => Some(AmbientSpot::birds(Vec::new())),
        Some(other) => {
            return Err(ambience_error(
                "ambience_unknown_preset",
                format!("unknown spot preset `{other}`"),
                "use water or birds",
            ));
        }
    };
    if let Some(bits) = p.get("entity") {
        let entity = entity_of(world, bits)?;
        if boolean(p, "remove")? == Some(true) {
            let mut e = world.entity_mut(entity);
            e.remove::<AmbientSpot>();
            e.remove::<regia_audio::AudioEmitter>();
            return Ok(json!({ "entity": entity.to_bits(), "created": false, "spot": null }));
        }
        let current = world.get::<AmbientSpot>(entity).cloned();
        let mut spot = match (base, current) {
            (Some(mut b), Some(c)) => {
                b.sounds = c.sounds;
                b
            }
            (Some(b), None) => b,
            (None, Some(c)) => c,
            (None, None) => AmbientSpot::water(String::new()),
        };
        apply_spot(&mut spot, p)?;
        if let Some(position) = p.get("position") {
            let at = vec3(position, "position")?;
            let mut e = world.entity_mut(entity);
            e.insert((
                Transform::from_translation(at),
                GlobalTransform(Transform::from_translation(at).compute_affine()),
            ));
        }
        let out = spot_json(&spot);
        world.entity_mut(entity).insert(spot);
        return Ok(json!({ "entity": entity.to_bits(), "created": false, "spot": out }));
    }
    let position = p
        .get("position")
        .ok_or_else(|| RpcError::invalid_params("pass `entity` to edit a spot, or `position` to create one"))
        .and_then(|v| vec3(v, "position"))?;
    let mut spot = base.unwrap_or_else(|| AmbientSpot::water(String::new()));
    apply_spot(&mut spot, p)?;
    let out = spot_json(&spot);
    let entity = world
        .spawn((
            Transform::from_translation(position),
            GlobalTransform(Transform::from_translation(position).compute_affine()),
            spot,
        ))
        .id();
    Ok(json!({ "entity": entity.to_bits(), "created": true, "spot": out }))
}

/// `audio.probe_ambience { listener, position, preset? | entity?, occlusion? }`: what a listener
/// hears of a spot (or a preset) sounding at a point. Moves nothing.
pub(crate) fn probe(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_audio(world)?;
    let p = obj(params)?;
    let listener = vec3(
        p.get("listener")
            .ok_or_else(|| RpcError::invalid_params("pass `listener`: [x, y, z]"))?,
        "listener",
    )?;
    let position = vec3(
        p.get("position")
            .ok_or_else(|| RpcError::invalid_params("pass `position`: [x, y, z]"))?,
        "position",
    )?;
    let mut spot = if let Some(bits) = p.get("entity") {
        let entity = entity_of(world, bits)?;
        world.get::<AmbientSpot>(entity).cloned().ok_or_else(|| {
            RpcError::not_found(
                format!("entity {} has no AmbientSpot", entity.to_bits()),
                "use audio.ambient_spots",
            )
        })?
    } else {
        match string(p, "preset")?.as_deref().unwrap_or("water") {
            "water" => AmbientSpot::water(String::new()),
            "birds" => AmbientSpot::birds(Vec::new()),
            other => {
                return Err(ambience_error(
                    "ambience_unknown_preset",
                    format!("unknown spot preset `{other}`"),
                    "use water or birds",
                ));
            }
        }
    };
    // Here `occlusion` is the amount between the two points, not the spot's occlusion switch.
    let mut fields = p.clone();
    fields.remove("occlusion");
    apply_spot(&mut spot, &fields)?;
    let occlusion = num(p, "occlusion", (0.0, 1.0))?.unwrap_or(0.0);
    let inputs = AmbienceInputs::from_weather(
        world.get_resource::<SceneWind>(),
        world.get_resource::<SurfaceWeather>(),
    );
    let tuning = world
        .get_resource::<AudioOcclusion>()
        .map(|o| o.tuning)
        .unwrap_or_default();
    let medium = world.resource::<Audio>().medium().clone();
    let hearing = spot_hearing(&spot, listener, position, occlusion, &medium, &tuning, &inputs);
    Ok(hearing_json(&hearing))
}

fn play_json(play: &FootstepPlay) -> Value {
    json!({
        "walker": play.walker,
        "surface": play.surface,
        "fallback": play.fallback,
        "variant": play.variant,
        "sound": play.sound,
        "volume_db": play.volume_db,
        "pitch": play.pitch,
        "position": [play.position.x, play.position.y, play.position.z],
        "started": play.started,
    })
}

fn bank_json(bank: &FootstepBank, sounds: Option<&SoundAssets>) -> Value {
    let sets: Map<String, Value> = bank
        .sets
        .iter()
        .map(|(name, set)| {
            let missing: Vec<&String> = set
                .sounds
                .iter()
                .filter(|s| sounds.is_none_or(|a| a.by_name(s).is_none()))
                .collect();
            (
                name.clone(),
                json!({
                    "sounds": set.sounds,
                    "volume_db": set.volume_db,
                    "pitch": set.pitch,
                    "pitch_jitter": set.pitch_jitter,
                    "volume_jitter_db": set.volume_jitter_db,
                    "missing_sounds": missing,
                }),
            )
        })
        .collect();
    json!({
        "sets": sets,
        "layer_surfaces": bank.layer_surfaces,
        "default_surface": bank.default_surface,
        "walk_speed_m_per_s": bank.walk_speed_m_per_s,
        "speed_db_per_doubling": bank.speed_db_per_doubling,
        "reference_weight_kg": bank.reference_weight_kg,
        "weight_db_per_doubling": bank.weight_db_per_doubling,
        "weight_pitch_exponent": bank.weight_pitch_exponent,
        "max_boost_db": bank.max_boost_db,
        "max_cut_db": bank.max_cut_db,
        "notify_prefix": bank.notify_prefix,
        "max_distance": bank.emitter.max_distance,
        "bus": bank.emitter.bus,
        "played": bank.played(),
        "recent": bank.recent().map(play_json).collect::<Vec<_>>(),
    })
}

/// `audio.footsteps`: `{ bank, tracker }` (`bank` null until `audio.set_footsteps`).
pub(crate) fn footsteps(world: &mut World) -> Result<Value, RpcError> {
    require_audio(world)?;
    let bank = world
        .get_resource::<FootstepBank>()
        .map(|b| bank_json(b, world.get_resource::<SoundAssets>()));
    let tracker = world
        .get_resource::<FootstepTracker>()
        .map(|t| json!({ "steps": t.steps, "ignored": t.ignored }));
    Ok(json!({ "bank": bank, "tracker": tracker }))
}

fn parse_set(value: &Value, surface: &str, current: Option<&FootstepSet>) -> Result<FootstepSet, RpcError> {
    let o = value
        .as_object()
        .ok_or_else(|| RpcError::invalid_params(format!("set `{surface}` must be an object or null")))?;
    let mut set = match (o.get("sounds"), current) {
        (Some(s), _) => FootstepSet {
            sounds: strings(s, "sounds")?,
            ..current.cloned().unwrap_or_else(|| FootstepSet::new(Vec::new()))
        },
        (None, Some(c)) => c.clone(),
        (None, None) => {
            return Err(RpcError::invalid_params(format!(
                "a new set `{surface}` needs `sounds`"
            )));
        }
    };
    if set.sounds.is_empty() || set.sounds.len() > 64 {
        return Err(RpcError::invalid_params(format!(
            "set `{surface}` needs 1 to 64 sounds (4 or more avoid an audible pattern)"
        )));
    }
    if let Some(v) = num(o, "volume_db", (-60.0, 12.0))? {
        set.volume_db = v;
    }
    if let Some(v) = num(o, "pitch", (0.25, 4.0))? {
        set.pitch = v;
    }
    if let Some(v) = num(o, "pitch_jitter", (0.0, 0.5))? {
        set.pitch_jitter = v;
    }
    if let Some(v) = num(o, "volume_jitter_db", (0.0, 12.0))? {
        set.volume_jitter_db = v;
    }
    Ok(set)
}

/// `audio.set_footsteps { enabled?, sets?, layer_surfaces?, default_surface?, ..., seed? }` →
/// `{ before, after }`.
pub(crate) fn set_footsteps(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_audio(world)?;
    let p = obj(params)?;
    let before = world
        .get_resource::<FootstepBank>()
        .map(|b| bank_json(b, world.get_resource::<SoundAssets>()));
    if boolean(p, "enabled")? == Some(false) {
        world.remove_resource::<FootstepBank>();
        return Ok(json!({ "before": before, "after": null }));
    }
    let mut bank = world.get_resource::<FootstepBank>().cloned().unwrap_or_default();
    if let Some(sets) = p.get("sets") {
        let sets = sets
            .as_object()
            .ok_or_else(|| RpcError::invalid_params("`sets` must be { surface: set | null }"))?;
        for (surface, value) in sets {
            if value.is_null() {
                bank.sets.remove(surface);
            } else {
                let set = parse_set(value, surface, bank.sets.get(surface))?;
                bank.sets.insert(surface.clone(), set);
            }
        }
    }
    if let Some(map) = p.get("layer_surfaces") {
        let map = map
            .as_object()
            .ok_or_else(|| RpcError::invalid_params("`layer_surfaces` must be { layer: surface | null }"))?;
        for (layer, surface) in map {
            match surface {
                Value::Null => {
                    bank.layer_surfaces.remove(layer);
                }
                Value::String(s) => {
                    bank.layer_surfaces.insert(layer.clone(), s.clone());
                }
                _ => {
                    return Err(RpcError::invalid_params(
                        "each layer maps to a surface name or null",
                    ));
                }
            }
        }
    }
    if let Some(s) = string(p, "default_surface")? {
        bank.default_surface = s;
    }
    if let Some(s) = string(p, "notify_prefix")? {
        if s.is_empty() {
            return Err(RpcError::invalid_params("`notify_prefix` must not be empty"));
        }
        bank.notify_prefix = s;
    }
    if let Some(v) = num(p, "walk_speed_m_per_s", (0.1, 20.0))? {
        bank.walk_speed_m_per_s = v;
    }
    if let Some(v) = num(p, "speed_db_per_doubling", (0.0, 12.0))? {
        bank.speed_db_per_doubling = v;
    }
    if let Some(v) = num(p, "reference_weight_kg", (1.0, 100_000.0))? {
        bank.reference_weight_kg = v;
    }
    if let Some(v) = num(p, "weight_db_per_doubling", (0.0, 12.0))? {
        bank.weight_db_per_doubling = v;
    }
    if let Some(v) = num(p, "weight_pitch_exponent", (0.0, 1.0))? {
        bank.weight_pitch_exponent = v;
    }
    if let Some(v) = num(p, "max_distance", (0.1, 1_000.0))? {
        bank.emitter.max_distance = v.max(bank.emitter.min_distance);
    }
    if let Some(bus) = string(p, "bus")? {
        bank.emitter.bus = Some(bus);
    }
    if let Some(seed) = p.get("seed") {
        bank.reseed(
            seed.as_u64()
                .ok_or_else(|| RpcError::invalid_params("`seed` must be a non-negative integer"))?,
        );
    }
    let after = bank_json(&bank, world.get_resource::<SoundAssets>());
    world.insert_resource(bank);
    Ok(json!({ "before": before, "after": after }))
}

/// `audio.footstep { position, walker?, surface?, speed_m_per_s?, weight_kg?, plan_only? }` →
/// the step played (or planned).
pub(crate) fn footstep(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_audio(world)?;
    let p = obj(params)?;
    let position = vec3(
        p.get("position")
            .ok_or_else(|| RpcError::invalid_params("pass `position`: [x, y, z]"))?,
        "position",
    )?;
    let walker = match p.get("walker") {
        None | Some(Value::Null) => 0,
        Some(v) => v
            .as_u64()
            .ok_or_else(|| RpcError::invalid_params("`walker` must be a non-negative integer"))?,
    };
    let plan_only = boolean(p, "plan_only")?.unwrap_or(false);
    if world.get_resource::<FootstepBank>().is_none() {
        return Err(RpcError::not_found(
            "no FootstepBank in this app",
            "enable footsteps with audio.set_footsteps { sets: { <surface>: { sounds: [...] } } }",
        ));
    }
    world.resource_scope(|world, mut bank: Mut<FootstepBank>| -> Result<Value, RpcError> {
        let event = FootstepEvent {
            walker,
            position,
            surface: string(p, "surface")?,
            speed_m_per_s: num(p, "speed_m_per_s", (0.0, 100.0))?.unwrap_or(bank.walk_speed_m_per_s),
            weight_kg: num(p, "weight_kg", (0.1, 100_000.0))?.unwrap_or(bank.reference_weight_kg),
        };
        let no_set = || {
            ambience_error(
                "footstep_no_set",
                "the bank has no set for this surface or its default surface",
                "add one with audio.set_footsteps { sets: { <surface>: { sounds: [...] } } }",
            )
        };
        let play = if plan_only {
            bank.plan(&event)
        } else {
            world
                .resource_scope(|world, mut audio: Mut<Audio>| {
                    bank.play(&mut audio, world.get_resource::<SoundAssets>(), &event)
                })
                .map_err(|e| RpcError::internal(format!("footstep did not start: {e}")))?
        };
        play.map(|p| play_json(&p)).ok_or_else(no_set)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_audio::{AudioPlugin, AudioSettings, SoundAsset, SoundClip};
    use regia_render::FoliageSway;
    use regia_scene::weather::SceneWindMode;

    fn app() -> App {
        let mut app = App::new();
        app.insert_resource(AudioSettings {
            headless: true,
            ..Default::default()
        });
        app.add_plugins((AudioPlugin, regia_framework::footsteps::FootstepPlugin));
        app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_secs_f64(1.0 / 60.0),
        ));
        let mut sounds = SoundAssets::default();
        for name in [
            "wind_loop",
            "rustle_loop",
            "air_loop",
            "bird_0",
            "bird_1",
            "bird_2",
            "water_loop",
        ] {
            let clip = SoundClip::new(vec![0.05; 9_600], 48_000).expect("a clip");
            sounds.add(name, SoundAsset::from_clip(&clip));
        }
        app.insert_resource(sounds);
        app
    }

    fn code(e: &RpcError) -> String {
        e.data
            .as_ref()
            .and_then(|d| d["code"].as_str())
            .unwrap_or_default()
            .to_string()
    }

    fn call(world: &mut World, f: fn(&mut World, &Value) -> Result<Value, RpcError>, params: Value) -> Value {
        f(world, &params).expect("the call succeeds")
    }

    /// The top of a default two-metre shrub: how far the sway stage moves it at `speed` (the CPU
    /// mirror of the vertex stage), the largest over one gust period.
    fn sway_m(speed: f32) -> f32 {
        let sway = FoliageSway::default();
        (0..40)
            .map(|i| {
                let t = i as f32 * 0.1;
                let o = sway.offset([0.0, 2.0, 0.0], [0.0, 1.0, 0.0], 1.0, [0.0; 3], t, [speed, 0.0]);
                (o[0] * o[0] + o[1] * o[1] + o[2] * o[2]).sqrt()
            })
            .fold(0.0, f32::max)
    }

    /// The test the brief asks for: the wind heard follows the wind drawn. Over a sweep of scene
    /// winds, still foliage means silent wind layers, barely moving foliage is barely audible,
    /// and the louder the wind the further the foliage moves.
    #[test]
    fn the_wind_heard_matches_the_foliage_drawn() {
        let mut app = app();
        call(app.world_mut(), set, json!({ "preset": "forest" }));
        let mut last: Option<(f32, f32)> = None;
        for speed in [0.0, 0.1, 0.25, 0.5, 1.0, 2.0, 4.0, 8.0, 12.0, 18.0] {
            app.world_mut().insert_resource(SceneWind {
                mode: SceneWindMode::Manual,
                velocity_m_per_s: [speed, 0.0],
                mean_m_per_s: [speed, 0.0],
            });
            // Ten seconds of frames: the bed settles (release 1.5 s).
            for _ in 0..600 {
                app.update();
            }
            let status = get(app.world_mut()).expect("status");
            let level = status["bed"]["wind_level_db"].as_f64().expect("a level") as f32;
            let sway = sway_m(speed);
            assert_eq!(status["coherence"]["agree"], json!(true), "{speed} m/s: {status}");
            if sway == 0.0 {
                assert!(
                    level <= MUTE_DB,
                    "still foliage, silent wind: {speed} m/s at {level} dB"
                );
            }
            if sway < 0.02 {
                assert!(
                    level < -45.0,
                    "foliage moving under 2 cm, no strong wind: {speed} m/s at {level} dB"
                );
            }
            if sway > 0.2 {
                assert!(
                    level > -30.0,
                    "foliage moving 20 cm is heard: {speed} m/s at {level} dB"
                );
            }
            if let Some((prev_sway, prev_level)) = last
                && sway > prev_sway
            {
                assert!(
                    level >= prev_level,
                    "more sway, never less wind: {speed} m/s {level} after {prev_level}"
                );
                if prev_level > MUTE_DB {
                    assert!(
                        level > prev_level,
                        "more sway, more wind: {speed} m/s {level} after {prev_level}"
                    );
                }
            }
            last = Some((sway, level));
        }
    }

    #[test]
    fn a_calm_scene_plays_the_bed_quietly_and_a_still_renderer_disagrees_with_a_loud_bed() {
        let mut app = app();
        call(
            app.world_mut(),
            set,
            json!({ "preset": "forest", "layers": [
                { "name": "wind", "sound": "wind_loop" },
                { "name": "air", "sound": "air_loop" },
                { "name": "rustle", "sound": "missing_loop" },
            ]}),
        );
        app.update();
        app.update();
        let status = get(app.world_mut()).expect("status");
        let layers = status["bed"]["layers"].as_array().expect("layers").clone();
        let layer = |n: &str| layers.iter().find(|l| l["name"] == n).cloned().expect("layer");
        assert_eq!(layer("wind")["playing"], json!(true));
        assert_eq!(layer("wind")["target_db"], json!(MUTE_DB), "no scene wind: calm");
        assert_eq!(layer("rustle")["missing_sound"], json!(true));
        assert_eq!(layer("rustle")["playing"], json!(false));
        assert_eq!(status["coherence"]["agree"], json!(true));
        assert!(status["foliage"].is_null(), "no renderer in this app");
        // A bed told the wind blows while the scene is still is reported as incoherent.
        app.world_mut().insert_resource(SceneWind::manual([10.0, 0.0]));
        for _ in 0..120 {
            app.update();
        }
        app.world_mut().insert_resource(SceneWind::CALM);
        let status = get(app.world_mut()).expect("status");
        assert_eq!(status["coherence"]["foliage_still"], json!(true));
        assert_eq!(status["coherence"]["agree"], json!(false), "{status}");
        // Disabling stops the voices and removes the bed.
        let off = call(app.world_mut(), set, json!({ "enabled": false }));
        assert!(off["after"].is_null());
        assert!(app.world().get_resource::<AmbienceBed>().is_none());
    }

    #[test]
    fn test_tones_give_every_forest_layer_a_playing_loop() {
        let mut app = app();
        call(
            app.world_mut(),
            set,
            json!({ "preset": "forest", "test_tones": true }),
        );
        app.update();
        let status = get(app.world_mut()).expect("status");
        for layer in status["bed"]["layers"].as_array().expect("layers") {
            assert_eq!(layer["playing"], json!(true), "{layer}");
            assert!(
                layer["sound"]
                    .as_str()
                    .is_some_and(|s| s.starts_with(TEST_TONE_PREFIX)),
                "{layer}"
            );
        }
        // Removing a layer stops its loop.
        let voice = {
            let bed = app.world().resource::<AmbienceBed>();
            bed.state[bed.layer_index("rain").expect("rain")]
                .voice
                .expect("a voice")
        };
        call(
            app.world_mut(),
            set,
            json!({ "layers": [{ "name": "rain", "remove": true }] }),
        );
        for _ in 0..60 {
            app.update();
            // The offline mixer advances the stop fade.
            let _ = app.world_mut().resource_mut::<Audio>().render_offline(800);
        }
        assert!(
            !app.world().resource::<Audio>().voice_alive(voice),
            "the removed layer's loop stopped"
        );
    }

    /// A host that sways the foliage through the renderer without a `SceneWind` (the showcase's
    /// `--foliage-wind`) leaves the bed calm: reported as a mismatch until the bed hears the same
    /// wind through `wind_override_m_per_s`.
    #[test]
    fn a_renderer_wind_without_a_scene_wind_needs_the_override() {
        let mut app = app();
        app.world_mut()
            .insert_resource(regia_render::RuntimeRenderer::default());
        app.world_mut()
            .resource_mut::<regia_render::RuntimeRenderer>()
            .0
            .set_foliage_wind(Some([6.0, 0.0]));
        call(app.world_mut(), set, json!({ "preset": "forest" }));
        for _ in 0..120 {
            app.update();
        }
        let status = get(app.world_mut()).expect("status");
        assert_eq!(status["foliage"]["still"], json!(false));
        assert_eq!(status["coherence"]["wind_silent"], json!(true));
        assert_eq!(status["coherence"]["agree"], json!(false), "{status}");
        call(
            app.world_mut(),
            set,
            json!({ "wind_override_m_per_s": [6.0, 0.0] }),
        );
        for _ in 0..300 {
            app.update();
        }
        let status = get(app.world_mut()).expect("status");
        assert_eq!(status["bed"]["wind_override_m_per_s"], json!([6.0, 0.0]));
        assert_eq!(status["coherence"]["agree"], json!(true), "{status}");
        assert!(
            status["bed"]["wind_level_db"].as_f64().expect("present") > -30.0,
            "{status}"
        );
        let err =
            set(app.world_mut(), &json!({ "wind_override_m_per_s": [1.0e9, 0.0] })).expect_err("too fast");
        assert_eq!(code(&err), "F0002");
    }

    #[test]
    fn set_ambience_refuses_bad_input_and_changes_nothing() {
        let mut app = app();
        let w = app.world_mut();
        let err = set(w, &json!({ "preset": "jungle" })).expect_err("unknown preset");
        assert_eq!(code(&err), "ambience_unknown_preset");
        let err = set(
            w,
            &json!({ "layers": [{ "name": "x", "drive": "thunder", "curve": [[0, 0]] }] }),
        )
        .expect_err("unknown drive");
        assert_eq!(code(&err), "ambience_unknown_drive");
        assert!(
            set(w, &json!({ "layers": [{ "name": "new" }] })).is_err(),
            "a new layer needs drive and curve"
        );
        assert!(set(w, &json!({ "calm_m_per_s": -1 })).is_err());
        assert!(
            w.get_resource::<AmbienceBed>().is_none(),
            "a failed call changes nothing"
        );
        let ok = call(
            w,
            set,
            json!({ "layers": [{ "name": "creek", "drive": "rain", "curve": [[0.5, -30], [10, -12]] }] }),
        );
        assert!(
            ok["after"]["layers"]
                .as_array()
                .expect("present")
                .iter()
                .any(|l| l["name"] == "creek")
        );
    }

    #[test]
    fn spots_place_calls_with_distance_lowpass_and_weather_gates() {
        let mut app = app();
        let birds = call(
            app.world_mut(),
            set_spot,
            json!({ "position": [30, 0, 0], "preset": "birds", "sounds": ["bird_0", "bird_1", "bird_2"],
                    "interval_secs": [0.5, 1.0], "seed": 4 }),
        );
        let water = call(
            app.world_mut(),
            set_spot,
            json!({ "position": [-10, 0, 0], "preset": "water", "sounds": ["water_loop"] }),
        );
        assert_eq!(birds["created"], json!(true));
        for _ in 0..600 {
            app.update();
        }
        let list = list_spots(app.world_mut()).expect("spots");
        let row = |id: &Value| {
            list["spots"]
                .as_array()
                .expect("present")
                .iter()
                .find(|s| s["entity"] == *id)
                .cloned()
                .expect("listed")
        };
        let b = row(&birds["entity"]);
        let calls = b["schedule"]["calls"].as_u64().expect("calls");
        assert!(
            (6..=21).contains(&calls),
            "a call every 0.5-1 s over 10 s: {calls}"
        );
        let w = row(&water["entity"]);
        assert_eq!(w["schedule"]["looping"], json!(true));
        assert!(
            app.world()
                .get::<regia_audio::AudioEmitter>(Entity::from_bits(
                    water["entity"].as_u64().expect("present")
                ))
                .is_some(),
            "the loop spot plays through an emitter"
        );
        // The far spot is quieter and duller than the near one.
        assert!(b["hearing"]["gain_db"].as_f64() < w["hearing"]["gain_db"].as_f64());
        // Rain silences the birds: no new calls.
        app.world_mut().insert_resource(SurfaceWeather {
            rain_mm_per_h: 5.0,
            ..SurfaceWeather::DRY
        });
        for _ in 0..300 {
            app.update();
        }
        let list2 = list_spots(app.world_mut()).expect("spots");
        let b2 = list2["spots"]
            .as_array()
            .expect("present")
            .iter()
            .find(|s| s["entity"] == birds["entity"])
            .cloned()
            .expect("present");
        assert_eq!(b2["schedule"]["calls"].as_u64(), Some(calls), "no call in rain");
        assert!(b2["schedule"]["skipped"].as_u64().expect("present") > 0);
        assert_eq!(b2["hearing"]["quieted"], json!(true));
        assert_eq!(
            app.world().resource::<AmbienceSpotState>().live_calls,
            0,
            "ended calls are despawned"
        );
    }

    #[test]
    fn probe_reports_distance_lowpass_and_occlusion() {
        let mut app = app();
        let w = app.world_mut();
        let near = call(w, probe, json!({ "listener": [0, 0, 0], "position": [5, 0, 0] }));
        let far = call(w, probe, json!({ "listener": [0, 0, 0], "position": [50, 0, 0] }));
        let hidden = call(
            w,
            probe,
            json!({ "listener": [0, 0, 0], "position": [5, 0, 0], "occlusion": 1.0 }),
        );
        assert!(far["gain_db"].as_f64() < near["gain_db"].as_f64());
        assert!(far["cutoff_hz"].as_f64() < near["cutoff_hz"].as_f64());
        assert!(hidden["cutoff_hz"].as_f64().expect("present") <= 700.5);
        let err = probe(
            w,
            &json!({ "listener": [0, 0, 0], "position": [1, 0, 0], "preset": "owl" }),
        )
        .expect_err("unknown preset");
        assert_eq!(code(&err), "ambience_unknown_preset");
    }

    #[test]
    fn footsteps_over_the_protocol_never_repeat_and_scale_with_speed() {
        let mut app = app();
        let w = app.world_mut();
        let err = footstep(w, &json!({ "position": [0, 0, 0] })).expect_err("no bank");
        assert_eq!(code(&err), "F0003");
        call(
            w,
            set_footsteps,
            json!({ "sets": { "grass": { "sounds": ["bird_0", "bird_1", "bird_2"] } },
                    "default_surface": "grass", "layer_surfaces": { "Meadow": "grass" }, "seed": 1 }),
        );
        let mut last = None;
        for _ in 0..50 {
            let play = call(w, footstep, json!({ "position": [0, 0, 0], "surface": "grass" }));
            assert_ne!(Some(play["variant"].clone()), last);
            assert_eq!(play["started"], json!(true));
            last = Some(play["variant"].clone());
        }
        let walk = call(
            w,
            footstep,
            json!({ "position": [0, 0, 0], "plan_only": true, "speed_m_per_s": 1.4 }),
        );
        let run = call(
            w,
            footstep,
            json!({ "position": [0, 0, 0], "plan_only": true, "speed_m_per_s": 5.6 }),
        );
        assert!(
            run["volume_db"].as_f64().expect("present") > walk["volume_db"].as_f64().expect("present") + 2.5
        );
        let status = footsteps(w).expect("status");
        assert_eq!(status["bank"]["played"], json!(50));
        assert_eq!(status["bank"]["layer_surfaces"]["Meadow"], json!("grass"));
        call(w, set_footsteps, json!({ "sets": { "grass": null } }));
        let err = footstep(w, &json!({ "position": [0, 0, 0] })).expect_err("no set");
        assert_eq!(code(&err), "footstep_no_set");
    }
}
