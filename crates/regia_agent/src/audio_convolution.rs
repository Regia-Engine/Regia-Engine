// SPDX-License-Identifier: MPL-2.0
//! Agent methods for convolution, time stretch and ambience files (ADR-2721,
//! `book/agents/audio-convolution.md`): `audio.bus_effects`, `audio.bus_effect_set`,
//! `audio.convolution_stats`, `audio.ambience_play`, `audio.ambience_stop`,
//! `audio.ambience_list` and `audio.time_stretch_probe`.
//!
//! A world without [`regia_audio::Audio`] returns `UNSUPPORTED` (`F0013`). Refusals carry the
//! engine's own codes (`ir_*`, `ambience_*`, `stretch_*`) as the Regia code.

use regia_app::prelude::*;
use regia_audio::{
    AmbienceDef, AmbienceError, Audio, BusEffect, ConvolutionError, ConvolutionTier, ImpulseResponse,
    TimeStretchError, time_stretch,
};
use serde_json::{Map, Value, json};

use crate::audio_mix::{fields, number, require_audio};
use crate::protocol::{self, RpcError};

fn string(fields: &Map<String, Value>, field: &str) -> Result<Option<String>, RpcError> {
    match fields.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(RpcError::invalid_params(format!("`{field}` must be a string"))),
    }
}

/// Every [`ConvolutionError::code`].
const CONVOLUTION_CODES: [&str; 9] = [
    "ir_empty",
    "ir_truncated",
    "ir_silent",
    "ir_non_finite",
    "ir_channels",
    "ir_sample_rate",
    "ir_too_long",
    "ir_tier_disabled",
    "convolver_busy",
];

fn ir_error(error: &ConvolutionError) -> RpcError {
    let kind = match error {
        ConvolutionError::Busy => protocol::INTERNAL_ERROR,
        _ => protocol::INVALID_PARAMS,
    };
    RpcError::new(
        kind,
        error.code(),
        error.to_string(),
        "see book/agents/audio-convolution.md for what each ir_* code needs",
    )
}

fn ambience_error(error: &AmbienceError) -> RpcError {
    RpcError::new(
        protocol::INVALID_PARAMS,
        error.code(),
        error.to_string(),
        "docs/formats/ambience-ron.md describes the file",
    )
}

fn stretch_error(error: &TimeStretchError) -> RpcError {
    RpcError::new(
        protocol::INVALID_PARAMS,
        error.code(),
        error.to_string(),
        "rates run from 0.5 (twice as long) to 2 (half as long)",
    )
}

fn effect_json(effect: &BusEffect) -> Value {
    match effect {
        BusEffect::Eq { .. } => json!({ "kind": "eq" }),
        BusEffect::LowPass { cutoff_hz, .. } => json!({ "kind": "low_pass", "cutoff_hz": cutoff_hz }),
        BusEffect::HighPass { cutoff_hz, .. } => json!({ "kind": "high_pass", "cutoff_hz": cutoff_hz }),
        BusEffect::Reverb {
            feedback,
            damping,
            mix,
            ..
        } => {
            json!({ "kind": "reverb", "feedback": feedback, "damping": damping, "mix": mix })
        }
        BusEffect::Delay { delay_secs, mix, .. } => {
            json!({ "kind": "delay", "delay_secs": delay_secs, "mix": mix })
        }
        BusEffect::Compressor {
            threshold_db, ratio, ..
        } => {
            json!({ "kind": "compressor", "threshold_db": threshold_db, "ratio": ratio })
        }
        BusEffect::Distortion { drive_db } => json!({ "kind": "distortion", "drive_db": drive_db }),
        BusEffect::Convolution {
            ir,
            mix,
            predelay_secs,
        } => {
            json!({ "kind": "convolution", "ir": ir, "mix": mix, "predelay_secs": predelay_secs })
        }
        // A bypassed effect (`audio-buses` v1, ADR-8354): the wrapped effect with `bypassed: true`.
        BusEffect::Bypassed(inner) => {
            let mut out = effect_json(inner);
            out["bypassed"] = json!(true);
            out
        }
    }
}

fn irs_json(audio: &Audio) -> Value {
    json!(
        audio
            .impulse_responses()
            .iter()
            .map(|ir| json!({
                "name": ir.name,
                "frames": ir.frames,
                "channels": ir.channels,
                "sample_rate": ir.sample_rate,
                "duration_secs": ir.duration_secs,
            }))
            .collect::<Vec<_>>()
    )
}

/// `audio.bus_effects`: every bus's effect chain from the layout (`bus` narrows it to one), the
/// registered impulse responses and the convolution tier.
pub fn bus_effects(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_audio(world)?;
    let fields = fields(params)?;
    let only = string(&fields, "bus")?;
    let audio = world.resource::<Audio>();
    let layout = audio.bus_layout();
    let mut buses = Vec::new();
    for bus in &layout.buses {
        buses.push((bus.name.clone(), false, bus.effects.clone()));
    }
    for send in &layout.send_buses {
        buses.push((send.name.clone(), true, send.effects.clone()));
    }
    if let Some(name) = &only
        && !buses.iter().any(|(b, _, _)| b == name)
    {
        return Err(RpcError::not_found(
            format!("no bus named `{name}`"),
            "audio.mix lists the buses",
        ));
    }
    let list: Vec<Value> = buses
        .iter()
        .filter(|(name, _, _)| only.as_ref().is_none_or(|o| o == name))
        .map(|(name, send, effects)| {
            json!({
                "bus": name,
                "send": send,
                "effects": effects.iter().map(effect_json).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(json!({
        "buses": list,
        "impulse_responses": irs_json(audio),
        "tier": audio.convolution_tier().name(),
        "max_ir_secs": audio.convolution_tier().max_ir_secs(),
    }))
}

/// `audio.bus_effect_set`: registers an impulse response from a project sound file (`ir_path`,
/// under the name `ir`), sends convolution `index` on `bus` an IR (`ir`, or `null` to fade the
/// wet path out) with `predelay_secs` over `fade_secs`, sets its `mix`, or sets the `tier`.
pub fn bus_effect_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_audio(world)?;
    let fields = fields(params)?;
    let ir = string(&fields, "ir")?;
    let ir_path = string(&fields, "ir_path")?;
    let bus = string(&fields, "bus")?;
    let index = number(&fields, "index", (0.0, 64.0))?.map_or(0, |n| n as usize);
    let predelay = number(&fields, "predelay_secs", (0.0, 0.5))?.unwrap_or(0.0) as f32;
    let fade = number(&fields, "fade_secs", (0.0, 10.0))?
        .map_or(regia_audio::DEFAULT_IR_CROSSFADE_SECS, |f| f as f32);
    let mix = number(&fields, "mix", (0.0, 1.0))?;
    let tier = match string(&fields, "tier")? {
        None => None,
        Some(name) => Some(ConvolutionTier::from_name(&name).ok_or_else(|| {
            RpcError::invalid_params(format!("unknown `tier` `{name}`: use low, mobile or desktop"))
        })?),
    };
    let mut registered = None;
    if let Some(path) = &ir_path {
        let name = ir.clone().unwrap_or_else(|| path.clone());
        let file = crate::audio::project_file(world, path)?;
        let bytes = std::fs::read(&file).map_err(|e| {
            RpcError::new(
                protocol::NOT_FOUND,
                "ir_unreadable",
                format!("`{path}` cannot be read: {e}"),
                "give a project-relative sound file (WAV, Ogg, FLAC)",
            )
        })?;
        let response = ImpulseResponse::from_encoded_bytes(&bytes).map_err(|e| ir_error(&e))?;
        world
            .resource_mut::<Audio>()
            .register_impulse_response(&name, response)
            .map_err(|e| ir_error(&e))?;
        registered = Some(name);
    }
    let mut audio = world.resource_mut::<Audio>();
    if let Some(tier) = tier {
        audio.set_convolution_tier(tier);
    }
    if let Some(bus) = &bus {
        let known = audio
            .convolution_stats()
            .iter()
            .any(|r| &r.bus == bus && r.index == index);
        if !known {
            return Err(RpcError::not_found(
                format!("bus `{bus}` has no convolution {index}"),
                "audio.bus_effects lists each bus's effects; audio.convolution_stats the convolvers",
            ));
        }
        if fields.contains_key("ir") || ir_path.is_some() {
            if let Some(name) = ir.as_deref().or(registered.as_deref())
                && !audio.impulse_responses().iter().any(|i| i.name == name)
            {
                return Err(RpcError::not_found(
                    format!("no impulse response named `{name}`"),
                    "register one with ir_path, or see audio.bus_effects impulse_responses",
                ));
            }
            let name = ir.as_deref().or(registered.as_deref());
            audio
                .set_bus_convolution(bus, index, name, predelay, fade)
                .map_err(|e| {
                    let text = e.to_string();
                    // `AudioError::Invalid` messages start with the convolution error's code.
                    let code = CONVOLUTION_CODES
                        .iter()
                        .find(|c| text.starts_with(*c))
                        .copied()
                        .unwrap_or("F0003");
                    RpcError::new(
                        protocol::INVALID_PARAMS,
                        code,
                        text,
                        "audio.convolution_stats shows the convolver",
                    )
                })?;
        }
        if let Some(mix) = mix {
            audio
                .set_bus_convolution_mix(bus, index, mix as f32)
                .map_err(|e| RpcError::invalid_params(e.to_string()))?;
        }
    }
    Ok(json!({
        "registered": registered,
        "tier": audio.convolution_tier().name(),
        "impulse_responses": irs_json(&audio),
    }))
}

/// `audio.convolution_stats`: every convolver (bus convolutions, then the zone convolver on
/// `ZoneIr`) with its IR, partitions, latency and click detector, plus the zone IR state.
pub fn convolution_stats(world: &mut World) -> Result<Value, RpcError> {
    require_audio(world)?;
    let mut audio = world.resource_mut::<Audio>();
    let reports: Vec<Value> = audio
        .convolution_stats()
        .iter()
        .map(|r| {
            json!({
                "bus": r.bus,
                "index": r.index,
                "ir": r.ir,
                "predelay_secs": r.predelay_secs,
                "blocks": r.stats.blocks,
                "swaps": r.stats.swaps,
                "crossfades_done": r.stats.crossfades_done,
                "crossfading": r.stats.crossfading,
                "ir_frames": r.stats.ir_frames,
                "ir_channels": r.stats.ir_channels,
                "partitions": r.stats.partitions,
                "latency_frames": r.stats.latency_frames,
                "wet_delay_frames": r.stats.wet_delay_frames,
                "mixer_rate": r.stats.mixer_rate,
                "rate_mismatch": r.stats.rate_mismatch,
                "mix": r.stats.mix,
                "max_step": r.stats.max_step,
            })
        })
        .collect();
    let (switches, fallbacks) = audio.reverb_zone_ir_counts();
    Ok(json!({
        "convolvers": reports,
        "zone_ir": audio.reverb_zone_ir(),
        "zone_ir_switches": switches,
        "zone_ir_fallbacks": fallbacks,
        "tier": audio.convolution_tier().name(),
        "partition_frames": regia_audio::CONVOLUTION_PARTITION,
        "impulse_responses": irs_json(&audio),
    }))
}

/// `audio.ambience_play`: plays an `.ambience.ron` from a project `path` or inline `text`
/// (replacing a playing ambience of the same name); `seed` overrides the file's.
pub fn ambience_play(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_audio(world)?;
    let fields = fields(params)?;
    let text = match (string(&fields, "path")?, string(&fields, "text")?) {
        (Some(path), None) => {
            let file = crate::audio::project_file(world, &path)?;
            std::fs::read_to_string(&file).map_err(|e| {
                RpcError::new(
                    protocol::NOT_FOUND,
                    "ambience_unreadable",
                    format!("`{path}` cannot be read: {e}"),
                    "give a project-relative .ambience.ron",
                )
            })?
        }
        (None, Some(text)) => text,
        _ => {
            return Err(RpcError::invalid_params(
                "give exactly one of `path` (a project .ambience.ron) or `text` (its RON)",
            ));
        }
    };
    if text.len() > 1 << 20 {
        return Err(ambience_error(&AmbienceError::Invalid(
            "the file is over 1 MiB".into(),
        )));
    }
    let mut def = AmbienceDef::from_ron(&text).map_err(|e| ambience_error(&e))?;
    if let Some(seed) = fields.get("seed") {
        def.seed = seed
            .as_u64()
            .ok_or_else(|| RpcError::invalid_params("`seed` must be a non-negative integer"))?;
    }
    let name = def.name.clone();
    let summary = json!({
        "name": name,
        "seed": def.seed,
        "beds": def.beds.iter().map(|b| b.name.clone()).collect::<Vec<_>>(),
        "one_shots": def.one_shots.iter().map(|s| s.name.clone()).collect::<Vec<_>>(),
    });
    world
        .resource_mut::<Audio>()
        .play_ambience(def)
        .map_err(|e| ambience_error(&e))?;
    Ok(summary)
}

/// `audio.ambience_stop`: stops the playing ambience `name` (its beds fade out).
pub fn ambience_stop(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_audio(world)?;
    let fields = fields(params)?;
    let name = string(&fields, "name")?.ok_or_else(|| RpcError::invalid_params("`name` is required"))?;
    if !world.resource_mut::<Audio>().stop_ambience(&name) {
        return Err(RpcError::new(
            protocol::NOT_FOUND,
            "ambience_not_playing",
            format!("no ambience named `{name}` is playing"),
            "audio.ambience_list lists the playing ambiences",
        ));
    }
    Ok(json!({ "stopped": name }))
}

/// `audio.ambience_list`: the playing ambiences with their clocks, bed levels and call counts.
pub fn ambience_list(world: &mut World) -> Result<Value, RpcError> {
    require_audio(world)?;
    let audio = world.resource::<Audio>();
    let list: Vec<Value> = audio
        .ambience_files()
        .iter()
        .map(|a| {
            json!({
                "name": a.name,
                "seed": a.seed,
                "clock_secs": a.clock_secs,
                "beds": a.beds.iter().map(|(n, db)| json!({ "name": n, "level_db": db })).collect::<Vec<_>>(),
                "one_shots": a.one_shots,
                "calls_played": a.calls_played,
                "calls_skipped": a.calls_skipped,
                "calls_missing": a.calls_missing,
            })
        })
        .collect();
    Ok(json!({ "ambiences": list }))
}

/// The frequency of a tone by interpolated rising zero crossings over its middle three quarters.
fn measured_hz(samples: &[f32], rate: f32) -> Option<f32> {
    let from = (samples.len() / 8).max(1);
    let to = samples.len() - samples.len() / 8;
    let mut first = None;
    let mut last = None;
    let mut count = 0u32;
    for i in from..to {
        let (a, b) = (samples[i - 1], samples[i]);
        if a < 0.0 && b >= 0.0 {
            let at = (i - 1) as f32 + a / (a - b);
            first.get_or_insert(at);
            last = Some(at);
            count += 1;
        }
    }
    let (first, last) = (first?, last?);
    (count > 1 && last > first).then(|| (count - 1) as f32 * rate / (last - first))
}

/// `audio.time_stretch_probe`: stretches a test tone (`hz`, `secs`) offline at `rate` and
/// reports the measured pitch and duration ratio and whether rate 1 is bit-identical; also
/// lists the playing time-stretched voices and, with `voice` and `rate`, retunes one.
pub fn time_stretch_probe(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_audio(world)?;
    let fields = fields(params)?;
    let rate = number(&fields, "rate", (0.0, 8.0))?.unwrap_or(1.0) as f32;
    let hz = number(&fields, "hz", (20.0, 8_000.0))?.unwrap_or(440.0) as f32;
    let secs = number(&fields, "secs", (0.1, 4.0))?.unwrap_or(1.0) as f32;
    regia_audio::check_rate(rate).map_err(|e| stretch_error(&e))?;
    if let Some(voice) = fields.get("voice") {
        let id = voice
            .as_u64()
            .ok_or_else(|| RpcError::invalid_params("`voice` must be a voice id"))?;
        world
            .resource_mut::<Audio>()
            .set_time_stretch_rate(id, rate)
            .map_err(|e| {
                RpcError::new(
                    protocol::NOT_FOUND,
                    "stretch_voice_not_found",
                    e.to_string(),
                    "voices lists the playing time-stretched voices",
                )
            })?;
    }
    let sample_rate = 48_000.0f32;
    let tone: Vec<f32> = (0..(secs * sample_rate) as usize)
        .map(|i| (std::f32::consts::TAU * hz * i as f32 / sample_rate).sin() * 0.5)
        .collect();
    let out = time_stretch(&tone, 1, rate).map_err(|e| stretch_error(&e))?;
    let identity = time_stretch(&tone, 1, 1.0).map_err(|e| stretch_error(&e))? == tone;
    let measured = measured_hz(&out, sample_rate);
    let duration_ratio = out.len() as f64 / tone.len() as f64;
    let voices: Vec<Value> = world
        .resource::<Audio>()
        .time_stretch_voices()
        .iter()
        .map(|v| {
            json!({ "id": v.id, "sound": v.label, "bus": v.bus, "rate": v.rate,
                    "position_secs": v.position_secs, "hops": v.hops })
        })
        .collect();
    Ok(json!({
        "rate": rate,
        "hz": hz,
        "measured_hz": measured,
        "pitch_error": measured.map(|m| (m / hz - 1.0).abs()),
        "duration_ratio": duration_ratio,
        "expected_ratio": 1.0 / f64::from(rate),
        "rate_one_identical": identity,
        "window_frames": regia_audio::WSOLA_WINDOW,
        "hop_frames": regia_audio::WSOLA_HOP,
        "voices": voices,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_audio::AudioSettings;

    fn world() -> World {
        let mut world = World::new();
        world.insert_resource(Audio::new(&AudioSettings {
            headless: true,
            ..AudioSettings::default()
        }));
        world
    }

    fn code(error: &RpcError) -> String {
        error
            .data
            .as_ref()
            .and_then(|d| d["code"].as_str())
            .unwrap_or_default()
            .to_string()
    }

    #[test]
    fn the_probe_keeps_the_pitch() {
        let mut world = world();
        for rate in [0.5, 0.75, 1.5, 2.0] {
            let out = time_stretch_probe(&mut world, &json!({ "rate": rate })).expect("probe");
            assert!(out["pitch_error"].as_f64().expect("measured") < 0.01, "{out}");
            let ratio = out["duration_ratio"].as_f64().expect("ratio");
            assert!((ratio * rate - 1.0).abs() < 0.01, "{out}");
            assert_eq!(out["rate_one_identical"], true);
        }
        let error = time_stretch_probe(&mut world, &json!({ "rate": 3.0 })).expect_err("too fast");
        assert_eq!(code(&error), "stretch_rate");
    }

    #[test]
    fn ambience_round_trip_through_the_methods() {
        let mut world = world();
        let text = r#"( version: 1, name: "test", seed: 9,
            one_shots: [( name: "a", sounds: ["x"], interval_secs: (1.0, 2.0), distance_m: (1.0, 5.0) )] )"#;
        let played = ambience_play(&mut world, &json!({ "text": text })).expect("play");
        assert_eq!(played["name"], "test");
        let list = ambience_list(&mut world).expect("list");
        assert_eq!(list["ambiences"][0]["name"], "test");
        ambience_stop(&mut world, &json!({ "name": "test" })).expect("stop");
        let error = ambience_stop(&mut world, &json!({ "name": "test" })).expect_err("gone");
        assert_eq!(code(&error), "ambience_not_playing");
        let error =
            ambience_play(&mut world, &json!({ "text": "( version: 3, name: \"x\" )" })).expect_err("v3");
        assert_eq!(code(&error), "ambience_version");
    }

    #[test]
    fn bus_effects_and_stats_report_the_zone_convolver() {
        let mut world = world();
        let effects = bus_effects(&mut world, &json!({ "bus": "Reverb" })).expect("effects");
        assert_eq!(effects["buses"][0]["effects"][0]["kind"], "reverb");
        let stats = convolution_stats(&mut world).expect("stats");
        assert_eq!(stats["convolvers"][0]["bus"], "ZoneIr");
        let error =
            bus_effect_set(&mut world, &json!({ "bus": "World", "ir": "x" })).expect_err("no convolution");
        assert_eq!(code(&error), "F0003");
        let set = bus_effect_set(&mut world, &json!({ "tier": "low" })).expect("tier");
        assert_eq!(set["tier"], "low");
    }
}
