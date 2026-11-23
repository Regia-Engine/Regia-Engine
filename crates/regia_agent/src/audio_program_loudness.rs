// SPDX-License-Identifier: MPL-2.0
//! `audio.program_loudness` (ADR-7227, `book/agents/audio-program-loudness.md`): the live EBU R128
//! meter of the whole output (`docs/specs/audio-loudness.md` § 6): momentary, short-term and gated
//! integrated loudness, loudness range and true peak since the meter was turned on or reset, read
//! lock-free from what the mixer thread publishes. The same meter feeds the editor mixer's program
//! row.
//!
//! A world without [`regia_audio::Audio`] returns `UNSUPPORTED` (`F0013`).

use regia_app::prelude::*;
use regia_audio::{Audio, ProgramLoudness};
use serde_json::{Value, json};

use crate::audio_mix::{boolean, fields, require_audio};
use crate::protocol::RpcError;

/// A finite figure as a JSON number; `−∞` (nothing measured yet) as `null`.
fn figure(value: f32) -> Value {
    if value.is_finite() {
        json!(value)
    } else {
        Value::Null
    }
}

/// The reading as the protocol reports it.
pub(crate) fn reading_json(r: &ProgramLoudness) -> Value {
    json!({
        "enabled": r.enabled,
        "seconds": r.seconds,
        "momentary_lufs": figure(r.momentary_lufs),
        "short_term_lufs": figure(r.short_term_lufs),
        "integrated_lufs": figure(r.integrated_lufs),
        "loudness_range_lu": figure(r.loudness_range_lu),
        "true_peak_db": figure(r.true_peak_db),
    })
}

/// `audio.program_loudness`: `enable` turns the meter on (a new measurement) or off, `reset` starts a
/// new measurement; both apply before the reading is taken. The reading is the one the mixer last
/// published (every 100 ms of output).
pub fn program_loudness(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_audio(world)?;
    let fields = fields(params)?;
    let enable = boolean(&fields, "enable")?;
    let reset = boolean(&fields, "reset")?.unwrap_or(false);
    let mut audio = world.resource_mut::<Audio>();
    if let Some(enable) = enable {
        audio.set_program_loudness_enabled(enable);
    }
    if reset {
        audio.reset_program_loudness();
    }
    let reading = audio.program_loudness();
    let mut out = reading_json(&reading);
    if let Value::Object(map) = &mut out {
        map.insert("headless".into(), json!(audio.is_headless()));
        map.insert("hop_ms".into(), json!(regia_audio::GATING_HOP_SECS * 1000.0));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_audio::{AudioPlugin, AudioSettings, LoopRegion, PlaybackParams, SoundAsset, SoundClip};

    fn app_world() -> World {
        let mut app = App::new();
        app.insert_resource(AudioSettings {
            headless: true,
            ..Default::default()
        });
        app.add_plugins(AudioPlugin);
        std::mem::take(app.world_mut())
    }

    fn tone(world: &mut World, amp: f32) {
        let samples: Vec<f32> = (0..48_000)
            .map(|i| (i as f32 * 1000.0 * std::f32::consts::TAU / 48_000.0).sin() * amp)
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
        audio.play_asset(&asset, params).expect("enqueue");
        audio.drain();
    }

    fn call(world: &mut World, params: Value) -> Value {
        crate::dispatch::call_in_process(world, "audio.program_loudness", params)
            .expect("the dispatcher routes audio.program_loudness")
            .expect("audio.program_loudness")
    }

    /// Through the dispatcher: off by default, `enable` turns it on, the reading after an offline
    /// render matches the offline loudness function on the same output, and `reset` starts again.
    #[test]
    fn the_dispatcher_meter_matches_the_offline_function_and_resets() {
        let mut world = app_world();
        let off = call(&mut world, json!({}));
        assert_eq!(off["enabled"], false, "{off}");
        assert_eq!(off["integrated_lufs"], Value::Null, "{off}");

        let on = call(&mut world, json!({ "enable": true }));
        assert_eq!(on["enabled"], true, "{on}");
        tone(&mut world, 0.1);
        // 5 s of output, a whole number of 100 ms hops.
        let out = world
            .resource_mut::<Audio>()
            .render_offline(5 * 48_000)
            .expect("headless render");
        let offline = regia_audio::measure_loudness(&out, 2, 48_000).expect("valid");
        let live = call(&mut world, json!({}));
        assert!(
            (live["seconds"].as_f64().expect("seconds") - 5.0).abs() < 1e-6,
            "{live}"
        );
        let integrated = live["integrated_lufs"].as_f64().expect("integrated");
        assert!(
            (integrated - f64::from(offline.integrated_lufs)).abs() < 0.05,
            "live {integrated} vs offline {}",
            offline.integrated_lufs
        );
        assert!(live["short_term_lufs"].as_f64().is_some(), "{live}");
        assert!(
            live["true_peak_db"].as_f64().expect("true peak") > -40.0,
            "{live}"
        );

        let reset = call(&mut world, json!({ "reset": true }));
        assert_eq!(reset["seconds"], 0.0, "{reset}");
        assert_eq!(reset["integrated_lufs"], Value::Null, "{reset}");

        let disabled = call(&mut world, json!({ "enable": false }));
        assert_eq!(disabled["enabled"], false, "{disabled}");
    }

    #[test]
    fn bad_params_are_refused() {
        let mut world = app_world();
        assert!(program_loudness(&mut world, &json!({ "enable": "yes" })).is_err());
        assert!(program_loudness(&mut world, &json!([1])).is_err());
        let mut bare = World::new();
        let err = program_loudness(&mut bare, &json!({})).expect_err("no audio");
        assert_eq!(err.code, crate::protocol::UNSUPPORTED);
        assert_eq!(err.data.as_ref().map(|d| d["code"].clone()), Some(json!("F0013")));
    }
}
