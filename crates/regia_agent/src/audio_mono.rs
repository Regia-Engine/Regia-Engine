// SPDX-License-Identifier: MPL-2.0
//! `audio.mono`: read or change the player's mono-audio choice (book `agents/audio-mono.md`,
//! ADR-2646). A change writes `GameSettings.accessibility.mono_audio` (persisted like
//! `settings.set`) and the live output stage of `Audio` at once.

use regia_audio::Audio;
use regia_ecs::prelude::*;
use regia_framework::settings::GameSettings;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, RpcError};

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Params {
    enabled: Option<bool>,
}

fn bad_params(detail: impl std::fmt::Display) -> RpcError {
    RpcError::new(
        INVALID_PARAMS,
        "audio_mono_bad_params",
        format!("bad `audio.mono` parameters: {detail}"),
        "pass { enabled: true } or { enabled: false }, or nothing to read",
    )
}

fn snapshot(world: &World) -> Value {
    let live = world.get_resource::<Audio>().map(Audio::mono_output);
    let saved = world
        .get_resource::<GameSettings>()
        .map(|s| s.accessibility.mono_audio);
    json!({
        "enabled": live.or(saved).unwrap_or(false),
        "live": live.is_some(),
        "persisted": saved.is_some(),
    })
}

/// `audio.mono { enabled? }`: with no parameters it only reports.
pub(crate) fn mono(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let params: Params = match params {
        Value::Null => Params::default(),
        Value::Object(_) => serde_json::from_value(params.clone()).map_err(bad_params)?,
        _ => return Err(bad_params("expected an object")),
    };
    if let Some(enabled) = params.enabled {
        if let Some(mut settings) = world.get_resource_mut::<GameSettings>()
            && settings.accessibility.mono_audio != enabled
        {
            settings.accessibility.mono_audio = enabled;
        }
        if let Some(mut audio) = world.get_resource_mut::<Audio>() {
            audio.set_mono_output(enabled);
        }
    }
    Ok(snapshot(world))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headless() -> Audio {
        Audio::new(&regia_audio::AudioSettings {
            headless: true,
            ..Default::default()
        })
    }

    #[test]
    fn reports_off_by_default() {
        let mut world = World::new();
        let out = mono(&mut world, &json!({})).expect("report");
        assert_eq!(
            out,
            json!({ "enabled": false, "live": false, "persisted": false })
        );
    }

    #[test]
    fn sets_the_output_stage_and_the_persisted_settings() {
        let mut world = World::new();
        world.insert_resource(GameSettings::default());
        world.insert_resource(headless());
        let out = mono(&mut world, &json!({ "enabled": true })).expect("set");
        assert_eq!(out, json!({ "enabled": true, "live": true, "persisted": true }));
        assert!(world.resource::<Audio>().mono_output());
        assert!(world.resource::<GameSettings>().accessibility.mono_audio);
        let out = mono(&mut world, &json!({ "enabled": false })).expect("set");
        assert_eq!(out["enabled"], json!(false));
        assert!(!world.resource::<Audio>().mono_output());
    }

    #[test]
    fn rejects_bad_params() {
        let mut world = World::new();
        world.insert_resource(GameSettings::default());
        for bad in [json!({ "enabled": 1 }), json!({ "on": true }), json!([true])] {
            let err = mono(&mut world, &bad).expect_err("bad");
            assert_eq!(err.data.expect("data")["code"], json!("audio_mono_bad_params"));
        }
        assert!(!world.resource::<GameSettings>().accessibility.mono_audio);
    }
}
