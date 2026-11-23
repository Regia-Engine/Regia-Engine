// SPDX-License-Identifier: MPL-2.0
//! Agent methods for music on the audio clock and for the listener (ADR-2517,
//! `book/agents/audio-music.md`): `audio.music_state`, `audio.music_transition`,
//! `audio.music_stinger` and `audio.listener_info`.
//!
//! A world without [`regia_audio::Audio`] returns `UNSUPPORTED` (`F0013`).

use regia_app::prelude::*;
use regia_audio::{
    Audio, AudioActiveCamera, AudioError, AudioListener, AudioListenerFollow, MusicSchedule, MusicSync,
    MusicTransitionRequest,
};
use serde_json::{Map, Value, json};

use crate::audio_mix::{fields, number, require_audio};
use crate::protocol::{self, RpcError};

fn required_string(fields: &Map<String, Value>, field: &str) -> Result<String, RpcError> {
    match fields.get(field) {
        Some(Value::String(s)) if !s.is_empty() => Ok(s.clone()),
        _ => Err(RpcError::invalid_params(format!(
            "`{field}` (a non-empty string) is required"
        ))),
    }
}

fn sync_param(fields: &Map<String, Value>) -> Result<Option<MusicSync>, RpcError> {
    match fields.get("sync") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(name)) => MusicSync::from_name(name).map(Some).ok_or_else(|| {
            RpcError::invalid_params(format!(
                "unknown `sync` `{name}`: use immediate, next_beat, next_bar or end_of_segment"
            ))
        }),
        Some(_) => Err(RpcError::invalid_params("`sync` must be a string")),
    }
}

fn schedule_json(schedule: &MusicSchedule) -> Value {
    json!({
        "name": schedule.name,
        "sync": schedule.sync.as_str(),
        "at_secs": schedule.at_secs,
        "at_frame": schedule.at_frame,
        "bar": schedule.bar,
        "beat": schedule.beat,
        "noop": schedule.noop,
    })
}

fn start_failed(error: &AudioError) -> RpcError {
    RpcError::new(
        protocol::INTERNAL_ERROR,
        "music_start_failed",
        error.to_string(),
        "a voice limit or a dead output; see logs.read and audio.mix",
    )
}

fn v3(v: regia_core::math::Vec3) -> Value {
    json!([v.x, v.y, v.z])
}

/// `audio.music_state`: the playing track, the audio clock, the bar and beat, the transition on
/// its way, and the names an agent can ask for.
pub fn music_state(world: &mut World) -> Result<Value, RpcError> {
    require_audio(world)?;
    let audio = world.resource::<Audio>();
    let state = audio.music_state();
    let sample_rate = audio.music_sample_rate();
    let stems: Vec<Value> = state
        .stems
        .iter()
        .map(|(name, db)| json!({ "name": name, "target_db": db }))
        .collect();
    let pending = state.pending.as_ref().map(|pending| {
        json!({
            "to": pending.to,
            "sync": pending.sync.as_str(),
            "switch_secs": pending.switch_secs,
            "start_secs": pending.start_secs,
            "crossfade_secs": pending.crossfade_secs,
            "segment": pending.segment,
        })
    });
    let stingers_playing: Vec<Value> = state
        .stingers_playing
        .iter()
        .map(|(name, at)| json!({ "name": name, "at_secs": at }))
        .collect();
    let clock_frame = state
        .clock_secs
        .map(|secs| (secs * f64::from(sample_rate)).round() as u64);
    Ok(json!({
        "track": state.track,
        "bpm": state.bpm,
        "beats_per_bar": state.beats_per_bar,
        "segment_beats": state.segment_beats,
        "clock_secs": state.clock_secs,
        "clock_frame": clock_frame,
        "sample_rate": sample_rate,
        "beat_index": state.beat_index,
        "bar": state.bar,
        "beat": state.beat,
        "intensity": state.intensity,
        "stems": stems,
        "pending": pending,
        "stingers_playing": stingers_playing,
        "crossfading": state.crossfading,
        "stingers": state.stingers,
        "tracks": state.tracks,
        "late_starts": state.late_starts,
    }))
}

/// `audio.music_transition`: moves to a registered track by the playing track's rule, with `sync`
/// and `crossfade_secs` over the rule.
pub fn music_transition(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_audio(world)?;
    let fields = fields(params)?;
    let to = required_string(&fields, "to")?;
    let sync = sync_param(&fields)?;
    let crossfade_secs = number(&fields, "crossfade_secs", (0.0, 600.0))?;
    let mut audio = world.resource_mut::<Audio>();
    audio
        .music_transition(&to, MusicTransitionRequest { sync, crossfade_secs })
        .map(|schedule| schedule_json(&schedule))
        .map_err(|error| match error {
            AudioError::UnknownEntry(_) => RpcError::new(
                protocol::NOT_FOUND,
                "music_unknown_track",
                format!("no music track named `{to}` is registered"),
                format!("registered tracks: {}", audio.music_state().tracks.join(", ")),
            ),
            other => start_failed(&other),
        })
}

/// `audio.music_stinger`: plays a stinger of the playing track on its sync, or on `sync`.
pub fn music_stinger(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_audio(world)?;
    let fields = fields(params)?;
    let name = required_string(&fields, "name")?;
    let sync = sync_param(&fields)?;
    let mut audio = world.resource_mut::<Audio>();
    if audio.current_music().is_none() {
        return Err(RpcError::new(
            protocol::INVALID_PARAMS,
            "music_not_playing",
            "no music track is playing",
            "start a track first with audio.music_transition",
        ));
    }
    audio
        .music_stinger(&name, sync)
        .map(|schedule| schedule_json(&schedule))
        .map_err(|error| match error {
            AudioError::UnknownEntry(_) => RpcError::new(
                protocol::NOT_FOUND,
                "music_unknown_stinger",
                format!("the playing track has no stinger named `{name}`"),
                format!("its stingers: {}", audio.music_state().stingers.join(", ")),
            ),
            AudioError::Invalid(message) => RpcError::new(
                protocol::INVALID_PARAMS,
                "music_not_playing",
                message,
                "the output was suspended or swapped: start a track again with audio.music_transition",
            ),
            other => start_failed(&other),
        })
}

/// `audio.listener_info`: where the ears are and why.
pub fn listener_info(world: &mut World) -> Result<Value, RpcError> {
    require_audio(world)?;
    let mut query = world.query_filtered::<(), With<AudioListener>>();
    let has_component = query.iter(world).next().is_some();
    let follow = world
        .get_resource::<AudioListenerFollow>()
        .copied()
        .unwrap_or_default();
    let camera = world
        .get_resource::<AudioActiveCamera>()
        .and_then(|camera| camera.0);
    let audio = world.resource::<Audio>();
    let source = if has_component {
        "component"
    } else if follow == AudioListenerFollow::ActiveCamera && camera.is_some() {
        "camera"
    } else {
        "origin"
    };
    let listeners: Vec<Value> = audio
        .listeners()
        .iter()
        .map(|(entity, frame)| {
            json!({
                "entity": entity.map(Entity::to_bits),
                "position": v3(frame.position),
                "forward": v3(frame.forward),
                "right": v3(frame.right),
                "up": v3(frame.up),
                "velocity": v3(frame.velocity),
                "hrtf": frame.hrtf,
            })
        })
        .collect();
    Ok(json!({
        "source": source,
        "follow": match follow {
            AudioListenerFollow::ActiveCamera => "active_camera",
            AudioListenerFollow::Off => "off",
        },
        "camera": camera.map(|pose| json!({ "entity": pose.entity.to_bits(), "position": v3(pose.position) })),
        "listeners": listeners,
    }))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use regia_audio::{
        AudioCameraPose, AudioPlugin, AudioSettings, MusicAsset, MusicStem, MusicStinger, SoundAsset,
        SoundClip,
    };
    use regia_core::math::{Quat, Vec3};

    use super::*;

    const BAR: usize = 96_000;

    fn level(value: f32, frames: usize) -> SoundAsset {
        SoundAsset::from_clip(&SoundClip::new(vec![value; frames], 48_000).expect("a valid clip"))
    }

    fn track(name: &str) -> MusicAsset {
        let mut asset = MusicAsset::from_stems(
            name,
            120.0,
            4,
            vec![MusicStem {
                name: "a".into(),
                asset: level(0.1, BAR),
                base_db: 0.0,
                intensity: Vec::new(),
            }],
        );
        asset.stingers.push(MusicStinger {
            name: "hit".into(),
            asset: level(0.5, 2_000),
            volume_db: 0.0,
            sync: MusicSync::NextBar,
        });
        asset
    }

    fn app() -> App {
        let mut app = App::new();
        app.insert_resource(AudioSettings {
            headless: true,
            ..Default::default()
        });
        app.add_plugins(AudioPlugin);
        app
    }

    fn render(app: &mut App, frames: usize) {
        let mut audio = app.world_mut().resource_mut::<Audio>();
        for _ in 0..frames / 800 {
            audio.tick(800.0 / 48_000.0);
            audio.render_offline(800).expect("the offline backend");
        }
    }

    fn code(error: &RpcError) -> String {
        error
            .data
            .as_ref()
            .and_then(|data| data["code"].as_str())
            .unwrap_or_default()
            .to_string()
    }

    #[test]
    fn no_audio_is_unsupported() {
        let mut world = World::new();
        for error in [
            music_state(&mut world).err(),
            music_transition(&mut world, &json!({ "to": "a" })).err(),
            music_stinger(&mut world, &json!({ "name": "a" })).err(),
            listener_info(&mut world).err(),
        ] {
            assert_eq!(code(&error.expect("an error")), "F0013");
        }
    }

    #[test]
    fn the_state_of_an_idle_system_is_empty() {
        let mut app = app();
        let state = music_state(app.world_mut()).expect("state");
        assert_eq!(state["track"], Value::Null);
        assert_eq!(state["clock_secs"], Value::Null);
        assert_eq!(state["pending"], Value::Null);
        assert_eq!(state["tracks"], json!([]));
    }

    #[test]
    fn an_agent_can_start_a_track_move_on_and_read_the_position() {
        let mut app = app();
        {
            let mut audio = app.world_mut().resource_mut::<Audio>();
            audio.register_music(track("combat"));
            audio.register_music(track("calm"));
        }
        let started = music_transition(app.world_mut(), &json!({ "to": "combat" })).expect("starts");
        assert_eq!(started["sync"], "immediate");
        render(&mut app, BAR / 2);
        let state = music_state(app.world_mut()).expect("state");
        assert_eq!(state["track"], "combat");
        assert_eq!(state["bpm"], 120.0);
        assert_eq!(state["beats_per_bar"], 4);
        assert_eq!(state["bar"], 0);
        assert_eq!(state["tracks"], json!(["calm", "combat"]));
        assert_eq!(state["stingers"], json!(["hit"]));
        assert_eq!(state["sample_rate"], 48_000);

        let moved = music_transition(
            app.world_mut(),
            &json!({ "to": "calm", "sync": "next_beat", "crossfade_secs": 0.25 }),
        )
        .expect("schedules");
        assert_eq!(moved["sync"], "next_beat");
        // The clock reads 47 200 here, one render step behind. Beat 2, at 48 000, is inside the 50 ms
        // lead, so the next beat is beat 3.
        assert_eq!(moved["at_frame"], 72_000);
        assert_eq!(moved["noop"], false);
        let state = music_state(app.world_mut()).expect("state");
        assert_eq!(state["pending"]["to"], "calm");
        assert_eq!(state["pending"]["crossfade_secs"], 0.25);
        let again = music_transition(app.world_mut(), &json!({ "to": "calm" })).expect("no-op");
        assert_eq!(again["noop"], true);

        render(&mut app, BAR);
        let state = music_state(app.world_mut()).expect("state");
        assert_eq!(state["track"], "calm");
        assert_eq!(state["pending"], Value::Null);
        assert_eq!(state["late_starts"], 0);
    }

    #[test]
    fn a_stinger_reports_where_it_lands() {
        let mut app = app();
        app.world_mut()
            .resource_mut::<Audio>()
            .play_music(&track("combat"), Duration::ZERO)
            .expect("plays");
        render(&mut app, BAR / 2);
        let hit = music_stinger(app.world_mut(), &json!({ "name": "hit" })).expect("schedules");
        assert_eq!(hit["at_frame"], BAR);
        assert_eq!(hit["bar"], 1);
        assert_eq!(hit["beat"], 0);
        assert_eq!(hit["sync"], "next_bar");
        let state = music_state(app.world_mut()).expect("state");
        assert_eq!(state["stingers_playing"][0]["name"], "hit");
    }

    #[test]
    fn bad_requests_get_their_documented_codes() {
        let mut app = app();
        let error = music_stinger(app.world_mut(), &json!({ "name": "hit" })).expect_err("nothing plays");
        assert_eq!(code(&error), "music_not_playing");
        let error = music_transition(app.world_mut(), &json!({ "to": "nope" })).expect_err("unknown");
        assert_eq!(code(&error), "music_unknown_track");
        app.world_mut()
            .resource_mut::<Audio>()
            .play_music(&track("combat"), Duration::ZERO)
            .expect("plays");
        let error = music_stinger(app.world_mut(), &json!({ "name": "nope" })).expect_err("unknown");
        assert_eq!(code(&error), "music_unknown_stinger");
        let error = music_transition(app.world_mut(), &json!({ "to": "nope" })).expect_err("unknown");
        assert_eq!(code(&error), "music_unknown_track");
        for bad in [
            json!({}),
            json!({ "to": "" }),
            json!({ "to": 5 }),
            json!({ "to": "a", "sync": "someday" }),
            json!({ "to": "a", "sync": 3 }),
            json!({ "to": "a", "crossfade_secs": -1 }),
            json!({ "to": "a", "crossfade_secs": "long" }),
        ] {
            let error = music_transition(app.world_mut(), &bad).expect_err("bad params");
            assert_eq!(code(&error), "F0002", "{bad}");
        }
    }

    #[test]
    fn the_listener_info_names_its_source() {
        let mut app = app();
        app.update();
        let info = listener_info(app.world_mut()).expect("info");
        assert_eq!(info["source"], "origin");
        assert_eq!(info["follow"], "active_camera");
        assert_eq!(info["camera"], Value::Null);

        let camera = app.world_mut().spawn_empty().id();
        app.insert_resource(AudioActiveCamera(Some(AudioCameraPose {
            entity: camera,
            position: Vec3::new(1.0, 2.0, 3.0),
            rotation: Quat::IDENTITY,
        })));
        app.update();
        let info = listener_info(app.world_mut()).expect("info");
        assert_eq!(info["source"], "camera");
        assert_eq!(info["camera"]["entity"], camera.to_bits());
        assert_eq!(info["listeners"][0]["position"], json!([1.0, 2.0, 3.0]));
        assert_eq!(info["listeners"][0]["forward"], json!([0.0, 0.0, -1.0]));

        app.insert_resource(AudioListenerFollow::Off);
        app.update();
        let info = listener_info(app.world_mut()).expect("info");
        assert_eq!(info["source"], "origin");
        assert_eq!(info["follow"], "off");

        let ears = app
            .world_mut()
            .spawn((AudioListener::default(), regia_scene::Transform::IDENTITY))
            .id();
        app.update();
        let info = listener_info(app.world_mut()).expect("info");
        assert_eq!(info["source"], "component");
        assert_eq!(info["listeners"][0]["entity"], ears.to_bits());
    }
}
