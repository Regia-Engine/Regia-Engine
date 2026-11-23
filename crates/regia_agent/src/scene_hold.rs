// SPDX-License-Identifier: MPL-2.0
//! `render.scene_hold` / `render.set_scene_hold` (ADR-7890, book/agents/frame-pacing.md "Paused
//! scene hold"): keep the last scene image and redraw only the UI, as a paused game does with
//! `GameFlowSettings::hold_scene_when_paused`.

use regia_app::prelude::*;
use regia_render::SceneHold;
use regia_render::scene_hold::{SceneHoldFrame, last_window_frame};
use serde_json::{Value, json};

use crate::protocol::RpcError;

fn missing() -> RpcError {
    RpcError::not_found(
        "no scene hold: this app has no SceneHold resource",
        "add regia_render::RenderPlugin",
    )
}

fn frame_json(frame: &SceneHoldFrame) -> Value {
    json!({
        "step": frame.step.name(),
        "held": frame.held,
        "scene_passes": frame.scene_views,
        "gpu_ms": frame.timed.then_some(frame.gpu_ms),
        "gpu_passes": frame
            .gpu_passes()
            .into_iter()
            .map(|(pass, ms)| json!({ "pass": pass, "ms": frame.timed.then_some(ms) }))
            .collect::<Vec<_>>(),
    })
}

fn state(world: &World) -> Result<Value, RpcError> {
    let hold = world.get_resource::<SceneHold>().ok_or_else(missing)?;
    let windows: Vec<Value> = world
        .get_resource::<regia_render::WindowSurfaces>()
        .map(|s| {
            s.scene_holds()
                .into_iter()
                .map(|(entity, frame, counts)| {
                    let mut row = frame_json(&frame);
                    row["window"] = json!(entity.to_bits());
                    row["held_frames"] = json!(counts.held_frames);
                    row["captures"] = json!(counts.captures);
                    row["full_frames"] = json!(counts.full_frames);
                    row
                })
                .collect()
        })
        .unwrap_or_default();
    let (seq, last) = last_window_frame();
    Ok(json!({
        "on": hold.is_on(),
        "refreshes": hold.refreshes(),
        "follow_pause": hold.follows_pause(),
        "paused": hold.paused(),
        "last_frame": (seq > 0).then(|| frame_json(&last)),
        "windows": windows,
    }))
}

/// `render.scene_hold`.
pub(crate) fn get(world: &World) -> Result<Value, RpcError> {
    state(world)
}

/// `render.set_scene_hold { on?, refresh?, follow_pause? }`.
pub(crate) fn set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let on = params.get("on");
    let refresh = params.get("refresh");
    let follow = params.get("follow_pause");
    if on.is_none() && refresh.is_none() && follow.is_none() {
        return Err(RpcError::invalid_params(
            "give `on` (a boolean), `refresh: true` or `follow_pause` (a boolean)",
        ));
    }
    let follow = match follow {
        None => None,
        Some(v) => Some(
            v.as_bool()
                .ok_or_else(|| RpcError::invalid_params("`follow_pause` must be a boolean"))?,
        ),
    };
    let on = match on {
        None => None,
        Some(v) => Some(
            v.as_bool()
                .ok_or_else(|| RpcError::invalid_params("`on` must be a boolean"))?,
        ),
    };
    let refresh = match refresh {
        None => false,
        Some(v) => v
            .as_bool()
            .ok_or_else(|| RpcError::invalid_params("`refresh` must be a boolean"))?,
    };
    let before = state(world)?;
    {
        let mut hold = world.get_resource_mut::<SceneHold>().ok_or_else(missing)?;
        if let Some(on) = on {
            hold.set(on);
        }
        if refresh {
            hold.refresh();
        }
        if let Some(follow) = follow {
            hold.set_follow_pause(follow);
        }
    }
    let after = state(world)?;
    Ok(json!({ "before": before, "after": after }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn a_world_without_the_resource_answers_not_found() {
        let mut world = World::new();
        assert!(get(&world).is_err());
        assert!(set(&mut world, &json!({ "on": true })).is_err());
    }

    #[test]
    fn set_turns_the_hold_on_and_off_and_asks_for_a_refresh() {
        let mut world = World::new();
        world.init_resource::<SceneHold>();
        let answer = set(&mut world, &json!({ "on": true })).unwrap();
        assert_eq!(answer["before"]["on"], false);
        assert_eq!(answer["after"]["on"], true);
        assert!(world.resource::<SceneHold>().is_on());
        let answer = set(&mut world, &json!({ "refresh": true })).unwrap();
        assert_eq!(answer["after"]["refreshes"], 1);
        assert_eq!(answer["after"]["on"], true, "a refresh keeps the hold on");
        set(&mut world, &json!({ "on": false })).unwrap();
        assert!(!world.resource::<SceneHold>().is_on());
        assert_eq!(
            get(&world).unwrap()["windows"],
            json!([]),
            "no windows in a bare world"
        );
    }

    /// ADR-10105: `follow_pause` makes the hold follow the gameplay pause.
    #[test]
    fn follow_pause_is_set_and_reported() {
        let mut world = World::new();
        world.init_resource::<SceneHold>();
        let answer = set(&mut world, &json!({ "follow_pause": true })).unwrap();
        assert_eq!(answer["before"]["follow_pause"], false);
        assert_eq!(answer["after"]["follow_pause"], true);
        assert_eq!(answer["after"]["on"], false, "not paused yet");
        world.resource_mut::<SceneHold>().note_paused(true);
        assert_eq!(get(&world).unwrap()["on"], true, "on while paused");
        assert!(set(&mut world, &json!({ "follow_pause": "yes" })).is_err());
    }

    #[test]
    fn bad_params_are_invalid() {
        let mut world = World::new();
        world.init_resource::<SceneHold>();
        assert!(set(&mut world, &json!({})).is_err());
        assert!(set(&mut world, &json!({ "on": "yes" })).is_err());
        assert!(set(&mut world, &json!({ "refresh": 1 })).is_err());
        assert!(!world.resource::<SceneHold>().is_on());
    }

    #[test]
    fn scene_hold_methods_round_trip_through_the_protocol() {
        let mut world = World::new();
        world.init_resource::<SceneHold>();
        let mut call = |method, params| {
            crate::dispatch::call_in_process(&mut world, method, params)
                .expect("synchronous method")
                .expect("protocol call")
        };
        assert_eq!(call("render.scene_hold", json!({}))["on"], false);
        assert_eq!(
            call("render.set_scene_hold", json!({ "on": true }))["after"]["on"],
            true
        );
        assert_eq!(call("render.scene_hold", json!({}))["on"], true);
        assert_eq!(
            call("render.set_scene_hold", json!({ "on": false }))["after"]["on"],
            false
        );
    }

    #[test]
    fn a_held_frame_lists_the_composite_and_the_ui_only() {
        let frame = SceneHoldFrame {
            step: regia_render::scene_hold::HoldStep::Held,
            held: true,
            scene_views: 0,
            composite_ms: 0.1,
            ui_ms: 0.2,
            gpu_ms: 0.3,
            timed: true,
        };
        let row = frame_json(&frame);
        assert_eq!(row["step"], "held");
        assert_eq!(row["scene_passes"], 0);
        let passes: Vec<&str> = row["gpu_passes"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|p| p["pass"].as_str())
            .collect();
        assert_eq!(passes, ["scene hold composite", "ui"]);
    }
}
