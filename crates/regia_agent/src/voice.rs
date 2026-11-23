// SPDX-License-Identifier: MPL-2.0
//! Agent methods for voice capture and playback (`docs/specs/16-audio.md` § 3b): the capture
//! stream's state (device, level, voice-activity, push-to-talk) and the voice loopback stream's
//! statistics, plus the controls to start/stop capture and the loopback and to set the
//! push-to-talk gate.
//!
//! `audio.voice` is read-only and always safe. The controls need [`regia_audio::Audio`] and
//! answer `UNSUPPORTED` (`F0013`) without it. `audio.capture_start` opens a real microphone: on
//! Android the `RECORD_AUDIO` permission must already be granted by the platform layer (Regia
//! Engine never prompts on its own), and on a device without an input — or in a headless run —
//! it answers with an error. Encoding and transport of voice chat are the game's job; Regia
//! Engine only captures and plays back.

use std::time::Duration;

use regia_app::prelude::*;
use regia_audio::voice_chat::VoicePlacement;
use regia_audio::{Audio, VoiceCaptureSettings, VoiceStreamSnapshot};
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

fn fields(params: &Value) -> Result<Map<String, Value>, RpcError> {
    match params {
        Value::Null => Ok(Map::new()),
        Value::Object(map) => Ok(map.clone()),
        _ => Err(RpcError::invalid_params("params must be an object")),
    }
}

fn number(fields: &Map<String, Value>, field: &str, range: (f64, f64)) -> Result<Option<f64>, RpcError> {
    let Some(value) = fields.get(field) else {
        return Ok(None);
    };
    let n = value
        .as_f64()
        .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be a number")))?;
    if !n.is_finite() || n < range.0 || n > range.1 {
        return Err(RpcError::invalid_params(format!(
            "`{field}` must be within {}..={}",
            range.0, range.1
        )));
    }
    Ok(Some(n))
}

fn boolean(fields: &Map<String, Value>, field: &str) -> Result<Option<bool>, RpcError> {
    match fields.get(field) {
        None => Ok(None),
        Some(value) => value
            .as_bool()
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be true or false"))),
    }
}

fn loopback_json(stats: &VoiceStreamSnapshot) -> Value {
    json!({
        "running": stats.running,
        "buffered_frames": stats.buffered_frames,
        "last_latency_ms": stats.last_latency_ms,
        "max_latency_ms": stats.max_latency_ms,
        "chunks_pushed": stats.chunks_pushed,
        "chunks_dropped": stats.chunks_dropped,
    })
}

/// The JSON form of a stream placement: `{ mode: center }`, `{ mode: unresolved }` or
/// `{ mode: spatial, gain, pan, cutoff_hz }`.
fn placement_json(placement: VoicePlacement) -> Value {
    match placement {
        VoicePlacement::Center => json!({ "mode": "center" }),
        VoicePlacement::Unresolved => json!({ "mode": "unresolved" }),
        VoicePlacement::Spatial { gain, pan, cutoff_hz } => json!({
            "mode": "spatial",
            "gain": gain,
            "pan": pan,
            "cutoff_hz": cutoff_hz,
        }),
    }
}

/// `audio.voice`: the capture stream and the voice loopback, if any — device, rate, channel
/// count, the input level `(rms, peak)` in dBFS, the voice-activity detector, the push-to-talk
/// gate, frames dropped by the capture ring, and the loopback stream's jitter buffer, latency
/// statistics and placement.
pub fn voice(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    require_audio(world)?;
    let audio = world.resource::<Audio>();
    let level = audio.capture_level_db();
    Ok(json!({
        "capturing": audio.is_capturing(),
        "device": audio.capture_device(),
        "sample_rate": audio.capture_sample_rate(),
        "channels": audio.capture_channels(),
        "level_db": level.map(|(rms, peak)| json!({ "rms": rms, "peak": peak })),
        "voice_active": audio.voice_active(),
        "talking": audio.is_talking(),
        "dropped_frames": audio.capture_dropped_frames(),
        "loopback": audio.voice_stats().map(|stats| {
            let mut value = loopback_json(&stats);
            if let Some(placement) = audio.voice_stream_placement() {
                value["placement"] = placement_json(placement);
            }
            value
        }),
    }))
}

/// `audio.capture_start`: opens the microphone and starts delivering mono frames. Omitted
/// settings use the defaults; the device may pick a different sample rate than requested (the
/// answer reports the rate and channel count actually opened). On Android the `RECORD_AUDIO`
/// permission must already be granted; with no input device this answers with an error.
pub fn capture_start(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_audio(world)?;
    let fields = fields(params)?;
    let device = match fields.get("device") {
        None => None,
        Some(value) => Some(
            value
                .as_str()
                .ok_or_else(|| RpcError::invalid_params("`device` must be a string"))?
                .to_string(),
        ),
    };
    let sample_rate = number(&fields, "sample_rate", (8_000.0, 192_000.0))?;
    let buffer_secs = number(&fields, "buffer_secs", (0.05, 30.0))?;
    let on_threshold_db = number(&fields, "on_threshold_db", (-90.0, 0.0))?;
    let off_threshold_db = number(&fields, "off_threshold_db", (-90.0, 0.0))?;
    let hangover_ms = number(&fields, "hangover_ms", (0.0, 5_000.0))?;
    let window_ms = number(&fields, "window_ms", (1.0, 2_000.0))?;

    let mut settings = VoiceCaptureSettings::default();
    if let Some(device) = device {
        settings.device = Some(device);
    }
    if let Some(value) = sample_rate {
        settings.sample_rate = value as u32;
    }
    if let Some(value) = buffer_secs {
        settings.buffer_secs = value as f32;
    }
    if let Some(value) = on_threshold_db {
        settings.vad.on_threshold_db = value as f32;
    }
    if let Some(value) = off_threshold_db {
        settings.vad.off_threshold_db = value as f32;
    }
    if let Some(value) = hangover_ms {
        settings.vad.hangover = Duration::from_secs_f64(value / 1000.0);
    }
    if let Some(value) = window_ms {
        settings.vad.window = Duration::from_secs_f64(value / 1000.0);
    }
    // Hysteresis needs `off < on`: otherwise a level between them toggles the gate every window
    // (speech chatters). Checked on the merged settings, so one given threshold is checked
    // against the other's default (ADR-1809).
    if settings.vad.off_threshold_db >= settings.vad.on_threshold_db {
        return Err(RpcError::invalid_params(format!(
            "`off_threshold_db` ({}) must be below `on_threshold_db` ({})",
            settings.vad.off_threshold_db, settings.vad.on_threshold_db
        )));
    }

    let mut audio = world.resource_mut::<Audio>();
    if audio.is_headless() {
        return Err(RpcError::new(
            protocol::UNSUPPORTED,
            "capture_headless",
            "voice capture does not run when audio is headless",
            "a headless Audio uses the offline output backend; capture needs a process with a real output device",
        ));
    }
    audio
        .start_capture(settings)
        .map_err(|e| RpcError::internal(e.to_string()))?;
    Ok(json!({
        "capturing": audio.is_capturing(),
        "device": audio.capture_device(),
        "sample_rate": audio.capture_sample_rate(),
        "channels": audio.capture_channels(),
    }))
}

/// `audio.capture_stop`: releases the microphone (a no-op when not capturing).
pub fn capture_stop(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    require_audio(world)?;
    let mut audio = world.resource_mut::<Audio>();
    audio.stop_capture();
    Ok(json!({ "capturing": false }))
}

/// `audio.set_talking`: opens or closes the push-to-talk gate when a capture exists. The reply
/// repeats the requested flag either way. With no capture the gate is not written, so
/// `audio.voice` stays `talking: false`, and a later `audio.capture_start` opens the gate.
/// With the gate closed, `Audio::tick_voice` updates the level meter and the VAD, then
/// returns an empty frame list: the samples are discarded.
pub fn set_talking(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_audio(world)?;
    let fields = fields(params)?;
    let talking = boolean(&fields, "talking")?
        .ok_or_else(|| RpcError::invalid_params("`talking` (true or false) is required"))?;
    world.resource_mut::<Audio>().set_talking(talking);
    Ok(json!({ "talking": talking }))
}

/// `audio.voice_loopback_start`: starts (or restarts) the voice loopback stream on the Voice bus
/// with a jitter buffer of `prebuffer_ms` (default 40). The game feeds it decoded remote frames
/// with [`regia_audio::Audio::push_voice_frames`].
pub fn voice_loopback_start(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_audio(world)?;
    let fields = fields(params)?;
    let prebuffer_ms = number(&fields, "prebuffer_ms", (0.0, 2_000.0))?.unwrap_or(40.0);
    let mut audio = world.resource_mut::<Audio>();
    audio
        .start_voice_loopback(prebuffer_ms as u32)
        .map_err(|e| RpcError::internal(e.to_string()))?;
    Ok(json!({ "loopback": audio.voice_stats().as_ref().map(loopback_json) }))
}

/// `audio.voice_loopback_stop`: stops the voice loopback stream (a no-op when not running).
pub fn voice_loopback_stop(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    require_audio(world)?;
    world.resource_mut::<Audio>().stop_voice_loopback();
    Ok(json!({ "loopback": Value::Null }))
}

/// `audio.voice_place { mode, gain?, pan?, cutoff_hz? }`: places the voice loopback stream
/// (spec 16 § 3b). `mode: "center"` restores the flat default; `mode: "spatial"` takes a
/// distance gain `0..=1` (default 1), a pan `-1..=1` (default 0) and an optional low-pass
/// cutoff in Hz (air absorption). Refused with `no_loopback` when no stream runs (starting a
/// new loopback resets the placement to center). A game normally lets a `VoiceStreamAnchor`
/// component drive this through the AudioSync pass — the method places the stream without an
/// entity, e.g. for a scripted sequence or a test.
pub fn voice_place(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_audio(world)?;
    let fields = fields(params)?;
    let mode = fields
        .get("mode")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`mode` must be \"center\" or \"spatial\""))?;
    let placement = match mode {
        "center" => VoicePlacement::Center,
        "spatial" => {
            let gain = number(&fields, "gain", (0.0, 1.0))?.unwrap_or(1.0);
            let pan = number(&fields, "pan", (-1.0, 1.0))?.unwrap_or(0.0);
            let cutoff_hz = number(&fields, "cutoff_hz", (1.0, 20_000.0))?;
            VoicePlacement::Spatial {
                gain: gain as f32,
                pan: pan as f32,
                cutoff_hz,
            }
        }
        other => {
            return Err(RpcError::invalid_params(format!(
                "`mode` must be \"center\" or \"spatial\", not \"{other}\""
            )));
        }
    };
    let mut audio = world.resource_mut::<Audio>();
    if audio.voice_stream_placement().is_none() {
        return Err(RpcError::new(
            protocol::INVALID_REQUEST,
            "no_loopback",
            "no voice loopback stream is running",
            "start one with audio.voice_loopback_start, then place it",
        ));
    }
    audio.set_voice_stream_placement(placement);
    Ok(json!({ "placement": placement_json(placement) }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_audio::AudioSettings;

    fn world_with_audio() -> World {
        let mut world = World::new();
        world.insert_resource(Audio::new(&AudioSettings {
            headless: true,
            ..AudioSettings::default()
        }));
        world
    }

    #[test]
    fn voice_methods_are_registered_in_the_schema() {
        for name in [
            "audio.voice",
            "audio.capture_start",
            "audio.capture_stop",
            "audio.set_talking",
            "audio.voice_loopback_start",
            "audio.voice_loopback_stop",
            "audio.voice_place",
        ] {
            assert!(crate::schema::method(name).is_some(), "{name} is in the schema");
        }
    }

    #[test]
    fn voice_status_reports_the_headless_capture() {
        let mut world = world_with_audio();
        let out = voice(&mut world, &json!({})).expect("audio.voice");
        assert_eq!(out["capturing"], false);
        assert_eq!(out["device"], "");
        assert_eq!(out["sample_rate"], 48_000);
        assert_eq!(out["channels"], 0);
        assert_eq!(out["level_db"], Value::Null);
        assert_eq!(out["voice_active"], false);
        assert_eq!(out["talking"], false);
        assert_eq!(out["dropped_frames"], 0);
        assert_eq!(out["loopback"], Value::Null);
    }

    #[test]
    fn voice_methods_need_the_audio_resource() {
        let mut empty = World::new();
        for params in [json!({}), Value::Null] {
            assert!(voice(&mut empty, &params).is_err());
            assert!(capture_start(&mut empty, &params).is_err());
            assert!(capture_stop(&mut empty, &params).is_err());
            assert!(set_talking(&mut empty, &params).is_err());
            assert!(voice_loopback_start(&mut empty, &params).is_err());
            assert!(voice_loopback_stop(&mut empty, &params).is_err());
            assert!(voice_place(&mut empty, &params).is_err());
        }
    }

    /// Specs/16 § 3b: `audio.voice_place` places the running loopback stream, `audio.voice`
    /// reports the placement, and the dispatcher path (envelope, permission, contract) answers.
    #[test]
    fn voice_place_places_the_stream_and_voice_reports_it() {
        fn code(err: &RpcError) -> String {
            err.data
                .as_ref()
                .and_then(|d| d.get("code"))
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| format!("{err:?}"))
        }
        let mut world = world_with_audio();
        // No stream running: the no_loopback code, documented on the book page.
        let err = voice_place(&mut world, &json!({ "mode": "spatial" })).expect_err("no stream");
        assert_eq!(code(&err), "no_loopback");

        // Through the dispatcher: start a headless loopback, place it, read it back.
        let start = crate::dispatch::call_in_process(
            &mut world,
            "audio.voice_loopback_start",
            json!({ "prebuffer_ms": 0 }),
        )
        .expect("answers")
        .expect("starts");
        assert_eq!(start["loopback"]["running"], true);
        let placed = crate::dispatch::call_in_process(
            &mut world,
            "audio.voice_place",
            json!({ "mode": "spatial", "gain": 0.8, "pan": -0.5 }),
        )
        .expect("answers")
        .expect("places");
        // The placement stores f32 gains; compare the fields, not the float rendering.
        assert_eq!(placed["placement"]["mode"], "spatial");
        assert!(
            (placed["placement"]["gain"].as_f64().expect("gain") - 0.8).abs() < 1e-6,
            "{placed}"
        );
        assert_eq!(placed["placement"]["pan"], -0.5);
        assert!(placed["placement"]["cutoff_hz"].is_null());
        let status = crate::dispatch::call_in_process(&mut world, "audio.voice", json!({}))
            .expect("answers")
            .expect("status");
        assert_eq!(status["loopback"]["placement"]["mode"], "spatial");
        assert_eq!(status["loopback"]["placement"]["pan"], -0.5);

        // Validation: mode required and named, ranges enforced.
        for bad in [
            json!({}),
            json!({ "mode": 5 }),
            json!({ "mode": "sideways" }),
            json!({ "mode": "spatial", "gain": 1.5 }),
            json!({ "mode": "spatial", "pan": -2.0 }),
            json!({ "mode": "spatial", "cutoff_hz": 0 }),
            json!({ "mode": "spatial", "cutoff_hz": "bright" }),
        ] {
            assert!(voice_place(&mut world, &bad).is_err(), "{bad}");
        }
        // Center restores the flat default.
        let centered = voice_place(&mut world, &json!({ "mode": "center" })).expect("center");
        assert_eq!(centered["placement"]["mode"], "center");
        let status = voice(&mut world, &json!({})).expect("status");
        assert_eq!(status["loopback"]["placement"]["mode"], "center");
    }

    #[test]
    fn controls_validate_parameters_before_touching_a_device() {
        let mut world = world_with_audio();
        assert!(capture_start(&mut world, &json!({ "sample_rate": -1 })).is_err());
        assert!(capture_start(&mut world, &json!({ "device": 5 })).is_err());
        // No hysteresis (ADR-1809): refused before any device opens.
        for params in [
            json!({ "on_threshold_db": -60.0, "off_threshold_db": -40.0 }),
            json!({ "on_threshold_db": -45.0, "off_threshold_db": -45.0 }),
            json!({ "on_threshold_db": -55.0 }),
        ] {
            let err = capture_start(&mut world, &params).expect_err("no hysteresis");
            assert!(
                err.message.contains("off_threshold_db"),
                "{params}: {}",
                err.message
            );
        }
        assert!(set_talking(&mut world, &json!({})).is_err());
        assert!(set_talking(&mut world, &json!({ "talking": "yes" })).is_err());
        assert!(voice_loopback_start(&mut world, &json!({ "prebuffer_ms": -1 })).is_err());
    }

    #[test]
    fn loopback_round_trips_and_reports_its_stream() {
        let mut world = world_with_audio();
        let started = voice_loopback_start(&mut world, &json!({ "prebuffer_ms": 10 }))
            .expect("loopback starts headless");
        assert_eq!(started["loopback"]["running"], true);
        assert_eq!(started["loopback"]["chunks_pushed"], 0);

        let seen = voice(&mut world, &json!({})).expect("audio.voice");
        assert_eq!(seen["loopback"]["running"], true);

        let stopped = voice_loopback_stop(&mut world, &json!({})).expect("loopback stops");
        assert_eq!(stopped["loopback"], Value::Null);
        let after = voice(&mut world, &json!({})).expect("audio.voice");
        assert_eq!(after["loopback"], Value::Null);
    }

    #[test]
    fn set_talking_and_capture_stop_are_noops_without_capture() {
        let mut world = world_with_audio();
        let out = set_talking(&mut world, &json!({ "talking": true })).expect("set_talking");
        assert_eq!(out["talking"], true);
        let seen = voice(&mut world, &json!({})).expect("audio.voice");
        assert_eq!(seen["talking"], false, "no capture, so the gate was not written");
        assert_eq!(seen["capturing"], false);
        let out = capture_stop(&mut world, &json!({})).expect("capture_stop");
        assert_eq!(out["capturing"], false);
    }

    #[test]
    fn set_talking_without_capture_does_not_open_the_gate() {
        let mut world = world_with_audio();
        set_talking(&mut world, &json!({ "talking": true })).expect("echoes the request");
        let seen = voice(&mut world, &json!({})).expect("audio.voice");
        assert_eq!(seen["talking"], false);
        assert_eq!(seen["capturing"], false);
    }

    #[test]
    fn capture_start_refuses_a_headless_audio() {
        let mut world = world_with_audio();
        let err = capture_start(&mut world, &json!({})).expect_err("headless");
        assert_eq!(
            err.data.as_ref().and_then(|d| d["code"].as_str()),
            Some("capture_headless")
        );
        assert_eq!(err.code, protocol::UNSUPPORTED);
        let seen = voice(&mut world, &json!({})).expect("audio.voice");
        assert_eq!(seen["capturing"], false);
        // A bad threshold is still reported before the headless refusal.
        let hysteresis = capture_start(
            &mut world,
            &json!({ "on_threshold_db": -45.0, "off_threshold_db": -45.0 }),
        )
        .expect_err("thresholds");
        assert!(
            hysteresis.message.contains("off_threshold_db"),
            "{}",
            hysteresis.message
        );
    }
}
