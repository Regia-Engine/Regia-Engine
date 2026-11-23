// SPDX-License-Identifier: MPL-2.0
//! Agent methods for loudness at import (ADR-1855, `book/agents/loudness-import.md`):
//! `audio.loudness` measures one sound and reports the normalization gain;
//! `audio.set_loudness_normalization` is the run-time switch over a cooked `loudness.ron`.
//!
//! A world without [`regia_audio::Audio`] returns `UNSUPPORTED` (`F0013`).

use regia_app::prelude::*;
use regia_audio::{
    LoudnessPolicy, LoudnessTable, LoudnessTableError, SoundAssets, SoundLoudness, measure_sound,
    measure_sound_bytes,
};
use serde_json::{Map, Value, json};

use crate::audio_mix::{boolean, fields, number, require_audio};
use crate::protocol::{self, RpcError};

fn string(fields: &Map<String, Value>, field: &str) -> Result<Option<String>, RpcError> {
    match fields.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(RpcError::invalid_params(format!("`{field}` must be a string"))),
    }
}

fn decode_failed(shown: &str, error: impl std::fmt::Display) -> RpcError {
    RpcError::new(
        protocol::INVALID_PARAMS,
        "loudness_decode_failed",
        format!("{shown}: {error}"),
        "the decoder reads WAV, Ogg Vorbis, FLAC and MP3",
    )
}

fn table_error(error: &LoudnessTableError) -> RpcError {
    let (code, hint) = match error {
        LoudnessTableError::Unreadable { .. } => (
            "loudness_table_unreadable",
            "pass a file; a cook writes loudness.ron into the cooked cache",
        ),
        LoudnessTableError::Invalid(_) => (
            "loudness_table_invalid",
            "cook the project again to rewrite loudness.ron",
        ),
        LoudnessTableError::UnsupportedVersion(_) => (
            "loudness_unsupported_version",
            "cook the project again with this build; the table is derived data",
        ),
    };
    RpcError::new(protocol::INVALID_PARAMS, code, error.to_string(), hint)
}

fn record_json(record: &SoundLoudness) -> Value {
    json!({
        "integrated_lufs": record.integrated_lufs,
        "loudness_range_lu": record.loudness_range_lu,
        "true_peak_db": record.true_peak_db,
        "short_clip": record.short_clip,
        "dual_mono": record.dual_mono,
        "sample_rate": record.sample_rate,
        "frames": record.frames,
        "duration_secs": record.duration_secs(),
        "target_lufs": record.target_lufs,
        "gain_db": record.gain_db,
        "peak_limited": record.peak_limited,
    })
}

/// `audio.loudness`: measures a registered sound (`sound`) or a project file (`path`) and reports
/// the gain the policy (the active table's, else the defaults; `target_lufs` replaces its target)
/// would give it. Decodes the whole sound, so it is a heavy method.
pub fn loudness(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_audio(world)?;
    let fields = fields(params)?;
    let (min, max) = LoudnessPolicy::TARGET_RANGE;
    let target = number(&fields, "target_lufs", (f64::from(min), f64::from(max)))?.map(|v| v as f32);
    let sound = string(&fields, "sound")?;
    let path = string(&fields, "path")?;
    let sounds = world.get_resource::<SoundAssets>();
    let policy = sounds
        .and_then(SoundAssets::loudness_normalization)
        .map_or_else(LoudnessPolicy::default, |table| table.policy);
    let (mut record, applied) = match (sound, path) {
        (Some(name), None) => {
            let asset = sounds.and_then(|s| s.by_name(&name)).ok_or_else(|| {
                RpcError::not_found(
                    format!("no sound named `{name}`"),
                    "audio.list_emitters lists the registered sounds",
                )
            })?;
            let record = measure_sound(asset).map_err(|e| decode_failed(&name, e))?;
            (record, Some(asset.loudness_gain_db()))
        }
        (None, Some(path)) => {
            let file = crate::audio::project_file(world, &path)?;
            let bytes = crate::req_path::read_file_capped(&file, crate::req_path::MAX_READ_BYTES)
                .map_err(|e| decode_failed(&path, e))?;
            let record = measure_sound_bytes(bytes, &path).map_err(|e| decode_failed(&path, e))?;
            (record, None)
        }
        _ => {
            return Err(RpcError::invalid_params(
                "give exactly one of `sound` (a registered name) or `path` (a project file)",
            ));
        }
    };
    record.apply_policy(&policy, true, target);
    let mut out = record_json(&record);
    if let Some(applied) = applied {
        out["applied_gain_db"] = json!(applied);
    }
    Ok(out)
}

/// `audio.set_loudness_normalization`: `enabled: true` loads `table` (a project-relative
/// `loudness.ron`) or keeps the active table; `enabled: false` turns normalization off. File sounds
/// take their new gain on their next play.
pub fn set_normalization(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_audio(world)?;
    let fields = fields(params)?;
    let enabled = boolean(&fields, "enabled")?
        .ok_or_else(|| RpcError::invalid_params("`enabled` (true or false) is required"))?;
    let table_path = string(&fields, "table")?;
    let table = match (enabled, table_path) {
        (false, _) => None,
        (true, Some(path)) => {
            let file = crate::audio::project_file(world, &path)?;
            Some(LoudnessTable::load(file).map_err(|e| table_error(&e))?)
        }
        (true, None) => {
            let active = world
                .get_resource::<SoundAssets>()
                .and_then(SoundAssets::loudness_normalization)
                .cloned();
            Some(active.ok_or_else(|| {
                RpcError::new(
                    protocol::INVALID_PARAMS,
                    "loudness_no_table",
                    "no loudness table is active and `table` was not given",
                    "pass `table`: the loudness.ron a cook writes into the cooked cache",
                )
            })?)
        }
    };
    let mut sounds = world.get_resource_or_insert_with(SoundAssets::default);
    let changed = sounds.set_loudness_normalization(table);
    let active = sounds.loudness_normalization();
    Ok(json!({
        "enabled": active.is_some(),
        "entries": active.map_or(0, |table| table.entries.len()),
        "changed": changed,
        "target_lufs": active.map(|table| table.policy.target_lufs),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_audio::{AudioPlugin, AudioSettings, REFERENCE_LUFS, SoundAsset, SoundClip};

    const RATE: u32 = 48_000;

    fn code(error: &RpcError) -> String {
        error
            .data
            .as_ref()
            .and_then(|d| d["code"].as_str())
            .unwrap_or_default()
            .to_string()
    }

    /// WAV (PCM16 mono) bytes of a 1 kHz sine.
    fn wav_bytes(seconds: f64, amp: f32) -> Vec<u8> {
        let len = (seconds * f64::from(RATE)) as usize;
        let mut data = Vec::with_capacity(len * 2);
        for i in 0..len {
            let s = ((i as f32 / RATE as f32) * 1000.0 * std::f32::consts::TAU).sin() * amp * 32_767.0;
            data.extend_from_slice(&(s as i16).to_le_bytes());
        }
        let mut wav = Vec::with_capacity(44 + data.len());
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&RATE.to_le_bytes());
        wav.extend_from_slice(&(RATE * 2).to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
        wav.extend_from_slice(&data);
        wav
    }

    /// A headless audio world over a temp project with `assets/quiet.wav` (registered as
    /// `quiet`) and a cooked-shaped `loudness.ron` that holds its record.
    fn project() -> (World, std::path::PathBuf) {
        let root = regia_test_utils::unique_temp_dir("agent-loudness");
        for dir in ["assets", "cooked"] {
            std::fs::create_dir_all(root.join(dir)).expect("project dirs");
        }
        let wav = wav_bytes(1.0, 0.02);
        regia_test_utils::write_atomic(&root.join("assets/quiet.wav"), &wav).expect("wav");
        let mut record = measure_sound_bytes(wav, "quiet").expect("measures");
        record.apply_policy(&LoudnessPolicy::default(), true, None);
        let mut table = LoudnessTable::default();
        table.entries.insert("assets/quiet.wav".into(), record);
        let text = table.to_ron_string().expect("ron");
        regia_test_utils::write_atomic(&root.join("cooked/loudness.ron"), text.as_bytes()).expect("table");

        let mut app = App::new();
        app.insert_resource(AudioSettings {
            headless: true,
            ..Default::default()
        });
        app.add_plugins(AudioPlugin);
        let server = regia_asset::AssetServer::open(regia_asset::AssetServerSettings {
            project_root: root.clone(),
            platform: "linux".into(),
            content_dir: None,
        })
        .expect("server");
        app.insert_resource(server);
        let mut world = std::mem::take(app.world_mut());
        world
            .resource_mut::<SoundAssets>()
            .add_file("quiet", root.join("assets/quiet.wav"))
            .expect("registers");
        (world, root)
    }

    #[test]
    fn loudness_measures_a_registered_sound_and_a_project_file_alike() {
        let (mut world, root) = project();
        let by_name = loudness(&mut world, &json!({ "sound": "quiet" })).expect("by name");
        let by_path = loudness(&mut world, &json!({ "path": "assets/quiet.wav" })).expect("by path");
        assert_eq!(by_name["integrated_lufs"], by_path["integrated_lufs"]);
        assert_eq!(by_name["gain_db"], by_path["gain_db"]);
        let measured = by_name["integrated_lufs"].as_f64().expect("loudness");
        let gain = by_name["gain_db"].as_f64().expect("gain");
        assert!(
            (measured + gain - f64::from(REFERENCE_LUFS)).abs() < 1e-3,
            "{by_name}"
        );
        assert_eq!(by_name["dual_mono"], true, "kira delivers mono as dual mono");
        assert_eq!(by_name["frames"], 48_000);
        assert_eq!(by_name["duration_secs"], 1.0);
        assert_eq!(by_name["applied_gain_db"], 0.0);
        assert!(by_path.get("applied_gain_db").is_none(), "{by_path}");

        let louder = loudness(&mut world, &json!({ "sound": "quiet", "target_lufs": -16 })).expect("target");
        assert_eq!(louder["target_lufs"], -16.0);
        assert!(louder["gain_db"].as_f64().expect("gain") > gain, "{louder}");
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn loudness_reports_silence_and_a_short_clip() {
        let (mut world, root) = project();
        let clip = SoundClip::new(vec![0.0; 48_000], RATE).expect("clip");
        let tone: Vec<f32> = (0..4_800)
            .map(|i| (i as f32 / RATE as f32 * 1000.0 * std::f32::consts::TAU).sin() * 0.25)
            .collect();
        let short = SoundClip::new(tone, RATE).expect("clip");
        {
            let mut sounds = world.resource_mut::<SoundAssets>();
            sounds.add("silence", SoundAsset::from_clip(&clip));
            sounds.add("blip", SoundAsset::from_clip(&short));
        }
        let silent = loudness(&mut world, &json!({ "sound": "silence" })).expect("silence");
        assert_eq!(silent["integrated_lufs"], Value::Null, "{silent}");
        assert_eq!(silent["gain_db"], 0.0);
        let blip = loudness(&mut world, &json!({ "sound": "blip" })).expect("blip");
        assert_eq!(blip["short_clip"], true, "{blip}");
        assert!(blip["integrated_lufs"].is_number(), "{blip}");
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn loudness_rejects_bad_input_with_its_codes() {
        let (mut world, root) = project();
        for bad in [
            json!({}),
            json!({ "sound": "quiet", "path": "assets/quiet.wav" }),
            json!({ "sound": 3 }),
            json!({ "sound": "quiet", "target_lufs": 3 }),
            json!({ "path": "" }),
            json!({ "path": "../quiet.wav" }),
            json!({ "path": root.join("assets/quiet.wav").display().to_string() }),
        ] {
            let error = loudness(&mut world, &bad).expect_err("bad params");
            assert_eq!(code(&error), "F0002", "{bad}");
        }
        let unknown = loudness(&mut world, &json!({ "sound": "nope" })).expect_err("unknown");
        assert_eq!(code(&unknown), "F0003");
        let missing = loudness(&mut world, &json!({ "path": "assets/nope.wav" })).expect_err("missing");
        assert_eq!(code(&missing), "F0003");
        regia_test_utils::write_atomic(&root.join("assets/bad.wav"), b"not audio").expect("bad");
        let garbage = loudness(&mut world, &json!({ "path": "assets/bad.wav" })).expect_err("garbage");
        assert_eq!(code(&garbage), "loudness_decode_failed");
        let mut empty = World::new();
        let none = loudness(&mut empty, &json!({ "sound": "quiet" })).expect_err("no audio");
        assert_eq!(code(&none), "F0013");
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn the_switch_applies_the_table_and_turns_off() {
        let (mut world, root) = project();
        let table = json!({ "enabled": true, "table": "cooked/loudness.ron" });
        let on = set_normalization(&mut world, &table).expect("on");
        assert_eq!(on["enabled"], true);
        assert_eq!(on["entries"], 1);
        assert_eq!(on["changed"], 1);
        assert_eq!(on["target_lufs"], f64::from(REFERENCE_LUFS));
        let read = loudness(&mut world, &json!({ "sound": "quiet" })).expect("read");
        assert_eq!(read["applied_gain_db"], read["gain_db"], "{read}");

        let keep = set_normalization(&mut world, &json!({ "enabled": true })).expect("keeps");
        assert_eq!(keep["enabled"], true);
        assert_eq!(keep["changed"], 0, "the same table changes nothing");

        let off = set_normalization(&mut world, &json!({ "enabled": false })).expect("off");
        assert_eq!(off["enabled"], false);
        assert_eq!(off["entries"], 0);
        assert_eq!(off["changed"], 1);
        assert_eq!(off["target_lufs"], Value::Null);
        let read = loudness(&mut world, &json!({ "sound": "quiet" })).expect("read");
        assert_eq!(read["applied_gain_db"], 0.0);
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn the_switch_rejects_bad_input_with_its_codes() {
        let (mut world, root) = project();
        for bad in [
            json!({}),
            json!({ "enabled": "yes" }),
            json!({ "enabled": true, "table": 3 }),
            json!({ "enabled": true, "table": "../loudness.ron" }),
        ] {
            let error = set_normalization(&mut world, &bad).expect_err("bad params");
            assert_eq!(code(&error), "F0002", "{bad}");
        }
        let no_table = set_normalization(&mut world, &json!({ "enabled": true })).expect_err("none active");
        assert_eq!(code(&no_table), "loudness_no_table");
        let missing = set_normalization(&mut world, &json!({ "enabled": true, "table": "nope.ron" }))
            .expect_err("gone");
        assert_eq!(code(&missing), "F0003");
        let directory =
            set_normalization(&mut world, &json!({ "enabled": true, "table": "assets" })).expect_err("dir");
        assert_eq!(code(&directory), "loudness_table_unreadable");
        regia_test_utils::write_atomic(&root.join("bad.ron"), b"not a table").expect("bad");
        let invalid =
            set_normalization(&mut world, &json!({ "enabled": true, "table": "bad.ron" })).expect_err("bad");
        assert_eq!(code(&invalid), "loudness_table_invalid");
        regia_test_utils::write_atomic(&root.join("new.ron"), b"(format_version: 99)").expect("new");
        let newer = set_normalization(&mut world, &json!({ "enabled": true, "table": "new.ron" }))
            .expect_err("newer");
        assert_eq!(code(&newer), "loudness_unsupported_version");
        assert!(
            world.resource::<SoundAssets>().loudness_normalization().is_none(),
            "a refused table leaves normalization off"
        );
        let mut empty = World::new();
        let none = set_normalization(&mut empty, &json!({ "enabled": false })).expect_err("no audio");
        assert_eq!(code(&none), "F0013");
        std::fs::remove_dir_all(root).expect("cleanup");
    }
}
