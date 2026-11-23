// SPDX-License-Identifier: MPL-2.0
//! Agent methods for the mixer (ADR-544, `book/agents/audio-mix.md`): `audio.mix` reports every
//! bus (layout, host and applied level, mute), the master limiter and the output meter;
//! `audio.set_bus` and `audio.set_limiter` change them; `audio.render_check` renders the mix
//! offline and measures it (peak, RMS, clipping, non-finite samples, the largest step — the
//! click detector), optionally writing a WAV to listen to.
//!
//! A world without [`regia_audio::Audio`] returns `UNSUPPORTED` (`F0013`).

use std::time::Duration;

use regia_app::prelude::*;
use regia_audio::{Audio, AudioSettings, BusStatus, MasterLimiter, OutputMeter, RenderStats, gain_to_db};
use serde_json::{Map, Value, json};

use crate::protocol::{self, RpcError};

/// The longest `audio.render_check` render, seconds.
const MAX_RENDER_SECS: f64 = 10.0;

pub(crate) fn require_audio(world: &World) -> Result<(), RpcError> {
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

pub(crate) fn fields(params: &Value) -> Result<Map<String, Value>, RpcError> {
    match params {
        Value::Null => Ok(Map::new()),
        Value::Object(map) => Ok(map.clone()),
        _ => Err(RpcError::invalid_params("params must be an object")),
    }
}

pub(crate) fn number(
    fields: &Map<String, Value>,
    field: &str,
    range: (f64, f64),
) -> Result<Option<f64>, RpcError> {
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

pub(crate) fn boolean(fields: &Map<String, Value>, field: &str) -> Result<Option<bool>, RpcError> {
    match fields.get(field) {
        None => Ok(None),
        Some(v) => v
            .as_bool()
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be true or false"))),
    }
}

fn bus_json(bus: &BusStatus) -> Value {
    json!({
        "name": bus.name,
        "parent": bus.parent,
        "layout_db": bus.layout_db,
        "base_db": bus.base_db,
        "target_db": bus.target_db,
        "current_db": bus.current_db,
        "applied_db": bus.applied_db,
        "muted": bus.muted,
        "bypass_medium": bus.bypass_medium,
        "sends": bus.sends.iter().map(|(name, db)| json!({ "bus": name, "volume_db": db })).collect::<Vec<_>>(),
    })
}

fn limiter_json(limiter: Option<MasterLimiter>) -> Value {
    match limiter {
        Some(l) => json!({
            "enabled": true,
            "ceiling_db": l.ceiling_db,
            "release_ms": f64::from(l.release_secs) * 1000.0,
        }),
        None => json!({ "enabled": false }),
    }
}

fn meter_json(meter: &OutputMeter) -> Value {
    json!({
        "frames": meter.frames,
        "peak_in_db": meter.peak_in_db(),
        "peak_out_db": meter.peak_out_db(),
        "would_clip": meter.peak_in > 1.0,
        "max_reduction_db": meter.max_reduction_db,
        "limited_frames": meter.limited_frames,
        "nonfinite_samples": meter.nonfinite_samples,
    })
}

/// `audio.mix`: every bus, the master limiter and the output meter. `reset_meter: true` starts
/// a new meter window after reading this one.
pub fn mix(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_audio(world)?;
    let fields = fields(params)?;
    let reset = boolean(&fields, "reset_meter")?.unwrap_or(false);
    let stall = world
        .get_resource::<regia_audio::AudioStallWatchdog>()
        .map_or(Value::Null, stall_json);
    let retry = world
        .get_resource::<regia_audio::AudioResumeRetry>()
        .copied()
        .unwrap_or_default();
    let mut audio = world.resource_mut::<Audio>();
    let output = json!({
        "suspended": audio.output_suspended(),
        "resume": retry.state(),
        "resume_failures": retry.failures(),
        "resume_retry_in_frames": retry.next_in_frames(),
    });
    let buses: Vec<Value> = audio.bus_status().iter().map(bus_json).collect();
    let out = json!({
        "buses": buses,
        "snapshot": audio.active_snapshot(),
        "limiter": limiter_json(audio.master_limiter()),
        "meter": meter_json(&audio.output_meter()),
        "headless": audio.is_headless(),
        "real_voices": audio.real_voice_count(),
        "virtual_voices": audio.virtual_voice_count(),
        "stall_watchdog": stall,
        "output": output,
    });
    if reset {
        audio.reset_output_meter();
    }
    Ok(out)
}

/// The output stall watchdog (ADR-8830): its settings and what it has done.
fn stall_json(dog: &regia_audio::AudioStallWatchdog) -> Value {
    json!({
        "enabled": dog.enabled,
        "stall_frames": dog.stall_frames,
        "min_stall_ms": u64::try_from(dog.min_stall.as_millis()).unwrap_or(u64::MAX),
        "max_backoff": dog.max_backoff,
        "recoveries": dog.recoveries(),
        "failed_resumes": dog.failed_resumes(),
        "frozen_frames": dog.frozen_frames(),
        "frozen_ms": u64::try_from(dog.frozen_time().as_millis()).unwrap_or(u64::MAX),
        "backoff": dog.backoff(),
        "retry_pending": dog.retry_pending(),
    })
}

fn bus_status(audio: &Audio, bus: &str) -> Option<BusStatus> {
    audio.bus_status().into_iter().find(|b| b.name == bus)
}

/// `audio.set_bus`: sets a bus's host volume (`volume_db`, faded over `fade_ms`) and/or its
/// mute. Omitted fields keep their value.
pub fn set_bus(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_audio(world)?;
    let fields = fields(params)?;
    let bus = fields
        .get("bus")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`bus` (a bus name) is required"))?
        .to_string();
    let volume_db = number(&fields, "volume_db", (-100.0, 24.0))?;
    let mute = boolean(&fields, "mute")?;
    let fade_ms = number(&fields, "fade_ms", (0.0, 10_000.0))?.unwrap_or(0.0);
    let mut audio = world.resource_mut::<Audio>();
    let Some(before) = bus_status(&audio, &bus) else {
        let names: Vec<String> = audio.bus_status().into_iter().map(|b| b.name).collect();
        return Err(RpcError::not_found(
            format!("no bus named `{bus}`"),
            format!("buses: {}", names.join(", ")),
        ));
    };
    if let Some(db) = volume_db {
        audio
            .set_bus_volume(&bus, db as f32, Duration::from_secs_f64(fade_ms / 1000.0))
            .map_err(|e| RpcError::internal(e.to_string()))?;
    }
    if let Some(mute) = mute {
        audio
            .set_bus_mute(&bus, mute)
            .map_err(|e| RpcError::internal(e.to_string()))?;
    }
    let after = bus_status(&audio, &bus).unwrap_or_else(|| before.clone());
    Ok(json!({ "before": bus_json(&before), "after": bus_json(&after) }))
}

/// `audio.set_limiter`: turns the master limiter on or off and sets its ceiling and release.
/// Turning it on without values uses the defaults (0 dBFS, 80 ms).
pub fn set_limiter(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_audio(world)?;
    let fields = fields(params)?;
    let enabled = boolean(&fields, "enabled")?;
    let ceiling_db = number(
        &fields,
        "ceiling_db",
        (f64::from(MasterLimiter::MIN_CEILING_DB), 0.0),
    )?;
    let release_ms = number(
        &fields,
        "release_ms",
        (
            f64::from(MasterLimiter::MIN_RELEASE_SECS) * 1000.0,
            f64::from(MasterLimiter::MAX_RELEASE_SECS) * 1000.0,
        ),
    )?;
    let mut audio = world.resource_mut::<Audio>();
    let before = audio.master_limiter();
    let on = enabled.unwrap_or(before.is_some() || ceiling_db.is_some() || release_ms.is_some());
    let next = on.then(|| {
        let base = before.unwrap_or_default();
        MasterLimiter {
            ceiling_db: ceiling_db.map_or(base.ceiling_db, |v| v as f32),
            release_secs: release_ms.map_or(base.release_secs, |v| (v / 1000.0) as f32),
        }
    });
    audio.set_master_limiter(next);
    Ok(json!({
        "before": limiter_json(before),
        "after": limiter_json(audio.master_limiter()),
    }))
}

fn channel_json(peak: f32, rms: f32) -> Value {
    json!({ "peak_db": gain_to_db(peak), "rms_db": gain_to_db(rms), "peak": peak, "rms": rms })
}

/// 16-bit PCM stereo WAV bytes of interleaved samples.
fn wav_bytes(interleaved: &[f32], sample_rate: u32) -> Vec<u8> {
    let data_len = (interleaved.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + interleaved.len() * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&2u16.to_le_bytes()); // stereo
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&(sample_rate * 4).to_le_bytes()); // byte rate
    out.extend_from_slice(&4u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for &s in interleaved {
        let s = if s.is_finite() { s.clamp(-1.0, 1.0) } else { 0.0 };
        out.extend_from_slice(&((s * 32_767.0).round() as i16).to_le_bytes());
    }
    out
}

/// `audio.render_check`: renders `seconds` of the current mix on the offline backend (game state
/// stays where it is: nothing ticks) and measures it. `wav: true` also writes the render as a
/// 16-bit stereo WAV into `out_dir` (default the scratch output dir — the system temp directory
/// under `regia_audio_mix` on desktop, the app-private data directory on Android).
pub fn render_check(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_audio(world)?;
    let fields = fields(params)?;
    let seconds = number(&fields, "seconds", (0.001, MAX_RENDER_SECS))?.unwrap_or(0.5);
    let wav = boolean(&fields, "wav")?.unwrap_or(false);
    // Confined like `audio.loopgen_render` and `seeing.dump_resource`, before anything renders.
    let wav_dir = if wav {
        Some(match fields.get("out_dir") {
            None | Some(Value::Null) => crate::scratch_out_dir(world, "regia_audio_mix"),
            Some(Value::String(dir)) => crate::audio::confined_out_dir(world, dir)?,
            Some(_) => return Err(RpcError::invalid_params("`out_dir` must be a string")),
        })
    } else {
        None
    };
    let sample_rate = world
        .get_resource::<AudioSettings>()
        .map_or(48_000, |s| s.sample_rate);
    let frames = (seconds * f64::from(sample_rate)).round() as usize;
    let mut audio = world.resource_mut::<Audio>();
    if !audio.is_headless() {
        return Err(RpcError::new(
            protocol::UNSUPPORTED,
            "F0013",
            "audio.render_check needs the offline backend",
            "run headless (AudioSettings.headless or no output device); on a device use audio.mix's meter",
        ));
    }
    audio.reset_output_meter();
    let (samples, stats): (Vec<f32>, RenderStats) = audio
        .render_offline_measured(frames)
        .map_err(|e| RpcError::internal(e.to_string()))?;
    let meter = audio.output_meter();
    let mut out = json!({
        "frames": stats.frames,
        "sample_rate": sample_rate,
        "peak_db": stats.peak_db(),
        "rms_db": stats.rms_db(),
        "left": channel_json(stats.peak_left, stats.rms_left),
        "right": channel_json(stats.peak_right, stats.rms_right),
        "max_step": stats.max_step,
        "max_step_ms": stats.max_step_frame as f64 * 1000.0 / f64::from(sample_rate),
        "clipped_samples": stats.clipped_samples,
        "nonfinite_samples": stats.nonfinite_samples,
        "clean": stats.is_clean() && meter.nonfinite_samples == 0,
        "meter": meter_json(&meter),
    });
    // EBU R128 integrated loudness and true peak of the rendered stereo mix (ADR-1563 g);
    // `null` when the render is shorter than one 400 ms gating block or silent.
    let loudness = regia_audio::measure_loudness(&samples, 2, sample_rate).ok();
    out["lufs"] = finite_json(loudness.map(|l| l.integrated_lufs));
    out["true_peak_db"] = finite_json(loudness.map(|l| l.true_peak_db));
    if let Some(dir) = wav_dir {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let file = format!("render-{}-{n}.wav", std::process::id());
        let written = crate::audio::write_atomic(&dir, &file, &wav_bytes(&samples, sample_rate))
            .map_err(|e| RpcError::internal(format!("writing {file}: {e}")))?;
        out["wav"] = json!(written.display().to_string());
    }
    Ok(out)
}

/// A finite reading as a JSON number, anything else (none, `−∞`, NaN) as `null`.
fn finite_json(value: Option<f32>) -> Value {
    value.filter(|v| v.is_finite()).map_or(Value::Null, |v| json!(v))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_audio::{AudioPlugin, LoopRegion, PlaybackParams, SoundAsset, SoundClip};

    fn app_world() -> World {
        let mut app = App::new();
        app.insert_resource(AudioSettings {
            headless: true,
            ..Default::default()
        });
        app.add_plugins(AudioPlugin);
        std::mem::take(app.world_mut())
    }

    fn loud_tone(world: &mut World, amp: f32) {
        let samples: Vec<f32> = (0..24_000)
            .map(|i| (i as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * amp)
            .collect();
        let asset = SoundAsset::from_clip(&SoundClip::new(samples, 48_000).expect("clip"));
        let params = PlaybackParams {
            bus: Some("Sfx".into()),
            loop_region: Some(LoopRegion {
                start_secs: 0.0,
                end_secs: None,
            }),
            ..PlaybackParams::default()
        };
        let mut audio = world.resource_mut::<Audio>();
        for _ in 0..4 {
            audio.play_asset(&asset, params.clone()).expect("enqueue");
        }
        audio.drain();
    }

    #[test]
    fn mix_lists_buses_limiter_and_meter() {
        let mut world = app_world();
        world.resource_mut::<Audio>().tick(1.0 / 60.0);
        let out = mix(&mut world, &json!({})).expect("mix");
        assert_eq!(out["buses"][0]["name"], "Master");
        assert_eq!(out["buses"][1]["name"], "Music");
        assert_eq!(out["buses"][1]["applied_db"], -3.0);
        assert_eq!(out["limiter"]["enabled"], true);
        assert_eq!(out["limiter"]["ceiling_db"], 0.0);
        assert_eq!(out["headless"], true);
        assert_eq!(out["meter"]["frames"], 0);
        assert_eq!(out["stall_watchdog"]["enabled"], true);
        assert_eq!(out["stall_watchdog"]["recoveries"], 0);
        assert_eq!(
            out["stall_watchdog"]["stall_frames"],
            regia_audio::AudioStallWatchdog::DEFAULT_STALL_FRAMES
        );
        assert_eq!(
            out["stall_watchdog"]["min_stall_ms"],
            regia_audio::AudioStallWatchdog::DEFAULT_MIN_STALL.as_millis() as u64
        );
        assert_eq!(out["stall_watchdog"]["frozen_ms"], 0);
        let mut empty = World::new();
        assert!(mix(&mut empty, &json!({})).is_err());
    }

    /// `audio.mix` reports the output stream and the engine's retry of a lifecycle resume that
    /// could not re-open it (deep-zai audio-mix finding 11), through the dispatcher.
    #[test]
    fn mix_reports_the_output_and_its_resume_retry() {
        let mut world = app_world();
        let read = |world: &mut World| {
            crate::dispatch::call_in_process(world, "audio.mix", json!({}))
                .expect("audio.mix answers at once")
                .expect("audio.mix")["output"]
                .clone()
        };
        let live = read(&mut world);
        assert_eq!(live["suspended"], false);
        assert_eq!(live["resume"], "idle");
        assert_eq!(live["resume_failures"], 0);
        assert!(live["resume_retry_in_frames"].is_null());
        world.resource_mut::<Audio>().suspend_output();
        assert_eq!(read(&mut world)["suspended"], true);
    }

    #[test]
    fn set_bus_changes_volume_and_mute_and_rejects_bad_input() {
        let mut world = app_world();
        let out = set_bus(
            &mut world,
            &json!({ "bus": "Music", "volume_db": -12, "mute": true }),
        )
        .expect("set");
        assert_eq!(out["before"]["base_db"], -3.0);
        assert_eq!(out["after"]["base_db"], -12.0);
        assert_eq!(out["after"]["muted"], true);
        let missing = set_bus(&mut world, &json!({ "bus": "Nope" })).expect_err("unknown bus");
        assert_eq!(missing.code, protocol::NOT_FOUND);
        assert!(set_bus(&mut world, &json!({ "bus": "Music", "volume_db": 99 })).is_err());
        assert!(set_bus(&mut world, &json!({ "volume_db": -3 })).is_err());
        assert!(set_bus(&mut world, &json!({ "bus": "Music", "mute": "yes" })).is_err());
    }

    #[test]
    fn set_limiter_round_trips_and_validates() {
        let mut world = app_world();
        let out = set_limiter(&mut world, &json!({ "ceiling_db": -1.0, "release_ms": 200 })).expect("set");
        assert_eq!(out["before"]["enabled"], true);
        let ceiling = out["after"]["ceiling_db"].as_f64().expect("ceiling");
        assert!((ceiling + 1.0).abs() < 1e-3, "{out}");
        let off = set_limiter(&mut world, &json!({ "enabled": false })).expect("off");
        assert_eq!(off["after"]["enabled"], false);
        assert!(world.resource::<Audio>().master_limiter().is_none());
        assert!(set_limiter(&mut world, &json!({ "ceiling_db": 3.0 })).is_err());
        assert!(set_limiter(&mut world, &json!({ "release_ms": 0 })).is_err());
    }

    #[test]
    fn render_check_measures_a_loud_mix_and_writes_a_wav() {
        let mut world = app_world();
        loud_tone(&mut world, 0.5);
        set_limiter(&mut world, &json!({ "enabled": false })).expect("bypass");
        let clipped = render_check(&mut world, &json!({ "seconds": 0.2 })).expect("render");
        assert_eq!(clipped["frames"], 9_600);
        assert!(
            clipped["clipped_samples"].as_u64().expect("count") > 1_000,
            "{clipped}"
        );
        assert_eq!(clipped["clean"], false);
        assert_eq!(clipped["meter"]["would_clip"], true);

        set_limiter(&mut world, &json!({ "enabled": true, "ceiling_db": -1.0 })).expect("on");
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("regia-audio-mix-agent-{}-{n}", std::process::id()));
        let limited = render_check(
            &mut world,
            &json!({ "seconds": 0.2, "wav": true, "out_dir": dir.display().to_string() }),
        )
        .expect("render");
        assert_eq!(limited["clipped_samples"], 0, "{limited}");
        // 0.2 s is shorter than one 400 ms gating block: no integrated loudness yet.
        assert_eq!(limited["lufs"], Value::Null, "{limited}");
        assert!(
            limited["true_peak_db"].as_f64().expect("true peak") <= 0.5,
            "{limited}"
        );
        assert_eq!(limited["clean"], true);
        assert!(limited["peak_db"].as_f64().expect("peak") <= -0.99);
        assert!(limited["meter"]["max_reduction_db"].as_f64().expect("reduction") > 4.0);
        let wav = std::fs::read(limited["wav"].as_str().expect("wav path")).expect("wav file");
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(wav.len(), 44 + 9_600 * 4);
        std::fs::remove_dir_all(&dir).expect("cleanup");

        assert!(render_check(&mut world, &json!({ "seconds": 60 })).is_err());
    }

    /// fixrev-05-k (audit-kimirev-fix-agent7-platform-muse, Med 1): with no `out_dir` the WAV
    /// must land under the scratch output dir — the system temp directory on desktop, the
    /// app-private data directory on Android — where the app can always write, and the reply must
    /// point at the file that exists.
    #[test]
    fn render_check_defaults_the_wav_to_the_scratch_out_dir() {
        let mut world = app_world();
        loud_tone(&mut world, 0.5);
        set_limiter(&mut world, &json!({ "enabled": false })).expect("bypass");
        let out = render_check(&mut world, &json!({ "seconds": 0.1, "wav": true })).expect("render");
        let want = crate::scratch_out_dir(&world, "regia_audio_mix");
        let wav = out["wav"].as_str().expect("wav path");
        assert!(
            wav.starts_with(&*want.to_string_lossy()),
            "wav at {wav}, expected under {}",
            want.display()
        );
        let bytes = std::fs::read(wav).expect("wav file");
        assert_eq!(&bytes[..4], b"RIFF");
        let _ = std::fs::remove_file(wav);
    }

    /// audit-agent-sec (High): `out_dir` was a raw path, so `wav: true` created directories and
    /// wrote a WAV anywhere the process could write.
    #[test]
    fn render_check_confines_the_wav_out_dir() {
        let mut world = app_world();
        let outside = std::path::Path::new("/usr/share/regia_audio_mix_refused");
        for bad in [
            json!(outside.display().to_string()),
            json!("../escape"),
            json!("a/../../b"),
            json!(""),
            json!(5),
        ] {
            let error = render_check(
                &mut world,
                &json!({ "seconds": 0.01, "wav": true, "out_dir": bad }),
            )
            .expect_err("refused");
            assert_eq!(error.code, crate::protocol::INVALID_PARAMS, "{bad}");
        }
        assert!(!outside.exists());
        // Without `wav` the directory is not used, so it is not checked either.
        assert!(render_check(&mut world, &json!({ "seconds": 0.01, "out_dir": "../x" })).is_ok());
        #[cfg(unix)]
        {
            let base = regia_test_utils::unique_temp_dir("audio-mix-outdir-link");
            std::os::unix::fs::symlink("/usr", base.join("link")).expect("symlink");
            let through = base.join("link").join("regia-audio-mix-refused");
            let error = render_check(
                &mut world,
                &json!({ "seconds": 0.01, "wav": true, "out_dir": through.display().to_string() }),
            )
            .expect_err("symlink out");
            assert_eq!(error.code, crate::protocol::INVALID_PARAMS);
            assert!(!through.exists());
            std::fs::remove_dir_all(&base).expect("cleanup");
        }
    }

    #[test]
    fn render_check_reports_the_mix_loudness() {
        let mut world = app_world();
        loud_tone(&mut world, 0.25);
        let out = render_check(&mut world, &json!({ "seconds": 1.0 })).expect("render");
        let lufs = out["lufs"]
            .as_f64()
            .expect("a 1 s render has an integrated loudness");
        // A 0.25-amplitude 440 Hz tone, panned center on two channels: well above the −70 LUFS
        // gate and below full scale.
        assert!(lufs > -40.0 && lufs < 0.0, "{out}");
        let true_peak = out["true_peak_db"].as_f64().expect("true peak");
        assert!(
            true_peak >= out["peak_db"].as_f64().expect("peak") - 0.01,
            "{out}"
        );
    }

    #[test]
    fn wav_bytes_clamp_and_silence_non_finite() {
        let bytes = wav_bytes(&[2.0, f32::NAN], 48_000);
        assert_eq!(&bytes[44..46], &32_767i16.to_le_bytes());
        assert_eq!(&bytes[46..48], &0i16.to_le_bytes());
    }
}
