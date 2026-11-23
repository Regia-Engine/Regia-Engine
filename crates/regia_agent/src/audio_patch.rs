// SPDX-License-Identifier: MPL-2.0
//! Agent methods for parametric audio patches (`.audiopatch.ron` v2, ADR-7226,
//! `book/agents/audio-patch.md`): `audio.patch_state` and `audio.patch_trigger`.
//!
//! A world without [`regia_audio::Audio`] returns `UNSUPPORTED` (`F0013`).

use regia_app::prelude::*;
use regia_audio::{Audio, AudioError};
use serde_json::{Value, json};

use crate::audio_mix::{fields, number, require_audio};
use crate::protocol::{self, RpcError};

/// `audio.patch_state`: every spawned patch with its parameters (value, target, binding) and
/// layers (kind, volume, pitch, one-shot and grain counts).
pub fn patch_state(world: &mut World) -> Result<Value, RpcError> {
    require_audio(world)?;
    let patches = world.resource::<Audio>().patch_state();
    serde_json::to_value(&patches)
        .map(|patches| json!({ "patches": patches }))
        .map_err(|e| RpcError::internal(e.to_string()))
}

/// `audio.patch_trigger`: fires a one-shot layer of a spawned patch now (its cooldown applies).
pub fn patch_trigger(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_audio(world)?;
    let fields = fields(params)?;
    let id = number(&fields, "id", (0.0, 9_007_199_254_740_991.0))?
        .ok_or_else(|| RpcError::invalid_params("`id` (a patch id from audio.patch_state) is required"))?;
    if id.fract() != 0.0 {
        return Err(RpcError::invalid_params("`id` must be a whole number"));
    }
    let layer = match fields.get("layer") {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        _ => {
            return Err(RpcError::invalid_params(
                "`layer` (a non-empty string) is required",
            ));
        }
    };
    let id = id as u64;
    let mut audio = world.resource_mut::<Audio>();
    match audio.patch_trigger(id, &layer) {
        Ok(fired) => {
            let state = audio.patch_state();
            let counts = state
                .iter()
                .find(|p| p.id == id)
                .and_then(|p| p.layers.iter().find(|l| l.name == layer))
                .map_or((0, 0), |l| (l.fired, l.skipped));
            Ok(
                json!({ "id": id, "layer": layer, "fired": fired, "total_fired": counts.0, "skipped": counts.1 }),
            )
        }
        Err(AudioError::UnknownEntry(message)) => Err(RpcError::new(
            protocol::NOT_FOUND,
            "patch_unknown",
            message,
            "audio.patch_state lists the spawned patches and their layers",
        )),
        Err(AudioError::Invalid(message)) => Err(RpcError::new(
            protocol::INVALID_PARAMS,
            "patch_not_one_shot",
            message,
            "only a layer with `one_shot` can be triggered; audio.patch_state shows each layer's kind",
        )),
        Err(other) => Err(RpcError::internal(other.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use regia_audio::{AudioPatch, AudioPatchDef, AudioPlugin, AudioSettings, SoundAsset, SoundClip};
    use serde_json::json;

    use super::*;

    fn code(error: &RpcError) -> String {
        error
            .data
            .as_ref()
            .and_then(|data| data["code"].as_str())
            .unwrap_or_default()
            .to_string()
    }

    fn patch() -> AudioPatch {
        let def = AudioPatchDef::from_ron(
            r#"(
            version: 2,
            name: "rover",
            parameters: [(name: "speed", min: 0.0, max: 30.0,
                          binding: Some((component: "Rover", field: "speed")))],
            layers: [
                (name: "motor", sound: Some("s")),
                (name: "horn", one_shot: Some((sounds: ["s"], trigger: Manual, cooldown_secs: 1.0))),
                (name: "gravel", granular: Some((sound: "s"))),
            ],
        )"#,
        )
        .expect("parses");
        let sounds: HashMap<&str, SoundAsset> = HashMap::from([(
            "s",
            SoundAsset::from_clip(&SoundClip::new(vec![0.1; 480], 48_000).expect("clip")),
        )]);
        AudioPatch::from_def(def, |path| {
            sounds
                .get(path)
                .cloned()
                .ok_or_else(|| AudioError::UnknownEntry(path.to_string()))
        })
        .expect("resolves")
    }

    #[test]
    fn no_audio_is_unsupported() {
        let mut world = World::new();
        for error in [
            patch_state(&mut world).err(),
            patch_trigger(&mut world, &json!({ "id": 1, "layer": "horn" })).err(),
        ] {
            assert_eq!(code(&error.expect("an error")), "F0013");
        }
    }

    #[test]
    fn bad_params_are_invalid() {
        let mut world = World::new();
        world.insert_resource(Audio::new(&AudioSettings {
            headless: true,
            ..Default::default()
        }));
        for params in [
            json!({}),
            json!({ "id": 1 }),
            json!({ "id": 1.5, "layer": "horn" }),
            json!({ "id": -1, "layer": "horn" }),
            json!({ "id": 1, "layer": "" }),
            json!([1]),
        ] {
            let error = patch_trigger(&mut world, &params).expect_err("invalid");
            assert_eq!(error.code, protocol::INVALID_PARAMS, "{params}");
        }
    }

    /// The dispatcher end to end: state lists the patch, a trigger fires the horn, a second one
    /// inside the cooldown is skipped, and the error codes are the documented ones.
    #[test]
    fn patch_methods_through_the_protocol_dispatcher() {
        use crate::dispatch::{AgentInbox, process_requests};
        use crate::protocol::{Request, Response};
        use crate::safety::{AgentConfig, AgentPermissions};
        use crate::server::Inbound;

        fn call(world: &mut World, method: &str, params: Value) -> Response {
            let (inbox_tx, inbox_rx) = crossbeam_channel::unbounded();
            let (reply_tx, reply_rx) = crossbeam_channel::bounded(1);
            inbox_tx
                .send(Inbound {
                    request: Request {
                        jsonrpc: "2.0".into(),
                        id: Some(json!(1)),
                        method: method.into(),
                        params,
                    },
                    reply: reply_tx,
                })
                .expect("queue request");
            world.insert_resource(AgentInbox(inbox_rx));
            process_requests(world);
            reply_rx.try_recv().expect("dispatcher answered")
        }

        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .insert_resource(AudioSettings {
                headless: true,
                ..Default::default()
            })
            .add_plugins(AudioPlugin)
            .insert_resource(AgentPermissions {
                config: AgentConfig {
                    rate_limit_per_second: 10_000,
                    ..AgentConfig::default()
                },
                source: None,
            })
            .init_resource::<crate::safety::AuditLog>()
            .init_resource::<crate::safety::AgentSession>();
        let id = app
            .world_mut()
            .resource_mut::<Audio>()
            .spawn_patch(patch())
            .expect("spawns");
        let world = app.world_mut();

        let state = call(world, "audio.patch_state", json!({}));
        assert!(state.error.is_none(), "{:?}", state.error);
        let state = state.result.expect("state");
        let patch = &state["patches"][0];
        assert_eq!(patch["id"], id);
        assert_eq!(patch["parameters"][0]["binding"], "Rover.speed");
        let kinds: Vec<&str> = patch["layers"]
            .as_array()
            .expect("layers")
            .iter()
            .map(|l| l["kind"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(kinds, ["loop", "one_shot", "granular"]);

        let first = call(world, "audio.patch_trigger", json!({ "id": id, "layer": "horn" }));
        assert!(first.error.is_none(), "{:?}", first.error);
        assert_eq!(first.result.expect("fired")["fired"], true);
        let second = call(world, "audio.patch_trigger", json!({ "id": id, "layer": "horn" }));
        let second = second.result.expect("answered");
        assert_eq!(
            (second["fired"].clone(), second["skipped"].clone()),
            (json!(false), json!(1))
        );

        let unknown = call(
            world,
            "audio.patch_trigger",
            json!({ "id": 999, "layer": "horn" }),
        );
        let error = unknown.error.expect("unknown patch");
        assert_eq!(error.data.as_ref().expect("data")["code"], "patch_unknown");
        let not_shot = call(
            world,
            "audio.patch_trigger",
            json!({ "id": id, "layer": "motor" }),
        );
        let error = not_shot.error.expect("a loop layer");
        assert_eq!(error.data.as_ref().expect("data")["code"], "patch_not_one_shot");
    }
}
