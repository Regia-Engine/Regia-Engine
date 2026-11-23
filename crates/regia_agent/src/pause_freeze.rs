// SPDX-License-Identifier: MPL-2.0
//! `play.freeze_state` (ADR-10105, book/agents/game-pause-freeze.md): what the gameplay pause holds
//! right now. `Time::paused` is the switch; this reports, per subsystem that runs on a clock of its
//! own, whether it follows it: the audio voices the pause holds, the scene hold, a threaded sim
//! host and the video players.

use regia_app::prelude::*;
use serde_json::{Value, json};

use crate::protocol::RpcError;

/// `play.freeze_state`.
pub(crate) fn freeze_state(world: &World) -> Result<Value, RpcError> {
    let time = world
        .get_resource::<Time>()
        .ok_or_else(|| RpcError::not_found("no Time resource", "add regia_app's MinimalPlugins"))?;
    let audio = world.get_resource::<regia_audio::Audio>().map(|audio| {
        json!({
            "game_paused": audio.game_paused(),
            "exempt_buses": audio.pause_exempt_buses(),
            "held_voices": audio.held_voice_count(),
            "held_total": audio.held_voice_total(),
        })
    });
    let scene_hold = world.get_resource::<regia_render::SceneHold>().map(|hold| {
        json!({
            "on": hold.is_on(),
            "follow_pause": hold.follows_pause(),
        })
    });
    let sim_host = world
        .get_resource::<regia_framework::sim_host::SimHostHandle>()
        .map(|handle| {
            let by_pause = world
                .get_resource::<regia_framework::sim_bridge::PauseHold>()
                .is_some_and(|latch| latch.held_by_pause);
            json!({ "held": handle.held(), "held_by_pause": by_pause })
        });
    let (players, held_players) =
        world
            .try_query::<&regia_media::VideoPlayerState>()
            .map_or((0usize, 0usize), |mut query| {
                query.iter(world).fold((0, 0), |(all, held), state| {
                    (all + 1, held + usize::from(state.core().game_held()))
                })
            });
    Ok(json!({
        "paused": time.paused,
        "frame": time.frame_count(),
        "audio": audio,
        "scene_hold": scene_hold,
        "sim_host": sim_host,
        "video": { "players": players, "held": held_players },
    }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn a_bare_world_reports_the_pause_and_null_subsystems() {
        let mut world = World::new();
        assert!(freeze_state(&world).is_err(), "no Time: not found");
        world.insert_resource(Time::default());
        world.resource_mut::<Time>().paused = true;
        world.init_resource::<regia_render::SceneHold>();
        world
            .resource_mut::<regia_render::SceneHold>()
            .set_follow_pause(true);
        world.resource_mut::<regia_render::SceneHold>().note_paused(true);
        let state = freeze_state(&world).unwrap();
        assert_eq!(state["paused"], true);
        assert_eq!(state["audio"], Value::Null);
        assert_eq!(state["sim_host"], Value::Null);
        assert_eq!(state["scene_hold"]["follow_pause"], true);
        assert_eq!(state["scene_hold"]["on"], true);
        assert_eq!(state["video"]["players"], 0);
    }

    #[test]
    fn the_method_round_trips_through_the_protocol() {
        let mut world = World::new();
        world.insert_resource(Time::default());
        let answer = crate::dispatch::call_in_process(&mut world, "play.freeze_state", json!({}))
            .expect("synchronous method")
            .expect("answers");
        assert_eq!(answer["paused"], false);
    }
}
