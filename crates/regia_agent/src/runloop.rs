// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface for the frame loop and gameplay sequencing (lane gameplay-coroutines):
//! `loop.get`, `loop.set` ([`LoopSettings`]), `quit.request` ([`regia_platform::request_quit`])
//! and `coroutines.list` ([`regia_framework::coroutine::Coroutines`]).

use regia_app::prelude::*;
use regia_app::{
    LoopControl, LoopMode, MAX_FIXED_HZ, MAX_PACING_FPS, MAX_SUBSTEPS_LIMIT, MAX_THROTTLED_FPS,
    MIN_PACING_FPS, MIN_THROTTLED_FPS,
};
use regia_framework::coroutine::{Coroutines, WaitingOn};
use serde_json::{Value, json};

use crate::protocol::RpcError;

fn in_range(value: f64, min: f64, max: f64) -> bool {
    value.is_finite() && (min..=max).contains(&value)
}

fn loop_json(world: &World) -> Value {
    let settings = LoopSettings::read(world);
    let control = world.get_resource::<LoopControl>().cloned().unwrap_or_default();
    let frames = match settings.mode {
        LoopMode::RunFrames(frames) => Some(frames),
        _ => None,
    };
    json!({
        "mode": settings.mode.name(),
        "frames": frames,
        "frames_in_mode": control.frames_in_mode(),
        "frozen": control.is_frozen(),
        "accepts_input": control.accepts_input(),
        "max_fps": settings.max_fps,
        "fixed_hz": settings.fixed_hz,
        "max_substeps": settings.max_substeps,
        "throttled_fps": settings.throttled_fps,
    })
}

/// `loop.get`: the loop mode and every pacing knob.
pub fn loop_get(world: &mut World) -> Result<Value, RpcError> {
    Ok(loop_json(world))
}

/// `loop.set`: changes any of `mode` (+ `frames` for `run_frames`), `max_fps` (`null` clears the cap,
/// and a render profile's own frame cap then applies again; else [`MIN_PACING_FPS`]`..=`[`MAX_PACING_FPS`]), `fixed_hz` (`1..=`[`MAX_FIXED_HZ`]),
/// `max_substeps` (`1..=`[`MAX_SUBSTEPS_LIMIT`]) and `throttled_fps`
/// ([`MIN_THROTTLED_FPS`]`..=`[`MAX_THROTTLED_FPS`]). A value outside that range is
/// `INVALID_PARAMS` and changes nothing. Returns `{ before, after }`.
pub fn loop_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let before = loop_json(world);
    let mut settings = LoopSettings::read(world);
    if let Some(mode) = params.get("mode") {
        let name = mode
            .as_str()
            .ok_or_else(|| RpcError::invalid_params("`mode` must be a string"))?;
        let frames = crate::params::opt_u64(params, "frames")?;
        settings.mode = LoopMode::from_name(name, frames).ok_or_else(|| {
            RpcError::invalid_params(format!(
                "unknown mode {name:?}: continuous | bench | lazy | run_frames (needs `frames`)"
            ))
        })?;
    }
    if let Some(max_fps) = params.get("max_fps") {
        settings.max_fps = match max_fps {
            Value::Null => None,
            value => Some(
                value
                    .as_f64()
                    .filter(|fps| in_range(*fps, f64::from(MIN_PACING_FPS), f64::from(MAX_PACING_FPS)))
                    .ok_or_else(|| {
                        RpcError::invalid_params(format!(
                            "`max_fps` must be a number in {}..={} or null",
                            MIN_PACING_FPS, MAX_PACING_FPS
                        ))
                    })? as f32,
            ),
        };
    }
    if let Some(hz) = params.get("fixed_hz") {
        settings.fixed_hz = hz
            .as_f64()
            .filter(|hz| hz.is_finite() && (1.0..=MAX_FIXED_HZ).contains(hz))
            .ok_or_else(|| {
                RpcError::invalid_params(format!("`fixed_hz` must be a number in 1..={MAX_FIXED_HZ}"))
            })?;
    }
    if let Some(steps) = params.get("max_substeps") {
        settings.max_substeps = steps
            .as_u64()
            .and_then(|s| u32::try_from(s).ok())
            .filter(|s| (1..=MAX_SUBSTEPS_LIMIT).contains(s))
            .ok_or_else(|| {
                RpcError::invalid_params(format!(
                    "`max_substeps` must be an integer in 1..={MAX_SUBSTEPS_LIMIT}"
                ))
            })?;
    }
    if let Some(fps) = params.get("throttled_fps") {
        settings.throttled_fps = fps
            .as_f64()
            .filter(|fps| in_range(*fps, f64::from(MIN_THROTTLED_FPS), f64::from(MAX_THROTTLED_FPS)))
            .ok_or_else(|| {
                RpcError::invalid_params(format!(
                    "`throttled_fps` must be a number in {}..={}",
                    MIN_THROTTLED_FPS, MAX_THROTTLED_FPS
                ))
            })? as f32;
    }
    settings.apply(world);
    Ok(json!({ "before": before, "after": loop_json(world) }))
}

/// `quit.request`: asks the app to quit. `mode` `clean` (default) quits the way closing its primary
/// window does, honouring [`regia_platform::ClosePolicy`]. `mode` `fast` is the host's decision
/// (no `ClosePolicy`): [`regia_platform::request_fast_exit`] hides every window now, then one frame
/// later (so this reply leaves the socket) the bounded flush runs and the process ends with code 0
/// without tearing the world down (ADR-8833). An unknown `mode` is `INVALID_PARAMS`.
pub fn quit_request(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let mode = match params.get("mode") {
        None | Some(Value::Null) => regia_platform::ExitMode::Clean,
        Some(Value::String(name)) => regia_platform::ExitMode::from_name(name)
            .ok_or_else(|| RpcError::invalid_params(format!("unknown mode {name:?}: clean | fast")))?,
        Some(_) => return Err(RpcError::invalid_params("`mode` must be a string: clean | fast")),
    };
    if mode == regia_platform::ExitMode::Fast {
        let request =
            regia_platform::request_fast_exit(world, 0, "agent quit.request", FAST_QUIT_DELAY_FRAMES);
        return Ok(json!({
            "window": Value::Null,
            "will_exit": true,
            "handled_by": "engine",
            "mode": mode.name(),
            "requested_frame": request.requested_frame,
            "flush_frame": request.flush_frame,
            "windows_hidden": request.windows_hidden,
        }));
    }
    let request = regia_platform::request_quit(world);
    Ok(json!({
        "window": request.window.map(Entity::to_bits),
        "will_exit": request.will_exit,
        "handled_by": if request.will_exit { "engine" } else { "game (ClosePolicy.exit_on_primary_close is off)" },
        "mode": mode.name(),
    }))
}

/// `quit.request { mode: "fast" }`: frames between the hide and the flush, so the reply is written
/// to the agent's socket before the process ends.
pub const FAST_QUIT_DELAY_FRAMES: u64 = 1;

/// `coroutines.list`: every live coroutine in start order, with what it waits on.
pub fn coroutines_list(world: &mut World) -> Result<Value, RpcError> {
    let Some(hub) = world.get_resource::<Coroutines>() else {
        return Err(RpcError::not_found(
            "no Coroutines resource in this app",
            "add regia_framework::coroutine::CoroutinePlugin and start coroutines with start_coroutine",
        ));
    };
    let rows: Vec<Value> = hub
        .list()
        .into_iter()
        .map(|info| {
            let (seconds_left, frames_left) = match info.waiting {
                WaitingOn::Seconds(secs) => (Some(secs), None),
                WaitingOn::Frames(frames) => (None, Some(frames)),
                _ => (None, None),
            };
            json!({
                "id": info.id.index(),
                "generation": info.id.generation(),
                "label": info.label,
                "owner": info.owner.map(Entity::to_bits),
                "started_frame": info.started_frame,
                "waiting": info.waiting.name(),
                "seconds_left": seconds_left,
                "frames_left": frames_left,
                "finished": info.finished,
            })
        })
        .collect();
    Ok(json!({ "count": rows.len(), "running": hub.len(), "coroutines": rows }))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use regia_app::{
        MAX_FIXED_HZ, MAX_PACING_FPS, MAX_SUBSTEPS_LIMIT, MAX_THROTTLED_FPS, MIN_PACING_FPS,
        MIN_THROTTLED_FPS,
    };

    use super::*;

    #[test]
    fn loop_set_round_trips_and_validates() {
        let mut app = App::new();
        let world = app.world_mut();
        let out = loop_set(
            world,
            &json!({ "mode": "run_frames", "frames": 5, "max_fps": 30, "fixed_hz": 120, "max_substeps": 2 }),
        )
        .expect("loop.set");
        assert_eq!(out["before"]["mode"], "continuous");
        assert_eq!(out["after"]["mode"], "run_frames");
        assert_eq!(out["after"]["frames"], 5);
        assert_eq!(out["after"]["max_fps"], 30.0);
        assert_eq!(out["after"]["fixed_hz"], 120.0);
        assert_eq!(out["after"]["max_substeps"], 2);
        let out = loop_set(world, &json!({ "mode": "bench", "max_fps": null })).expect("bench");
        assert_eq!(out["after"]["accepts_input"], false);
        assert_eq!(out["after"]["max_fps"], Value::Null);
        // Each documented endpoint is stored unchanged. One step past it is refused.
        let ends = loop_set(
            world,
            &json!({
                "max_fps": MAX_PACING_FPS,
                "fixed_hz": 1.0,
                "max_substeps": MAX_SUBSTEPS_LIMIT,
                "throttled_fps": MAX_THROTTLED_FPS,
            }),
        )
        .expect("endpoints");
        assert_eq!(ends["after"]["max_fps"], json!(MAX_PACING_FPS));
        assert_eq!(ends["after"]["fixed_hz"], 1.0);
        assert_eq!(ends["after"]["max_substeps"], MAX_SUBSTEPS_LIMIT);
        assert_eq!(ends["after"]["throttled_fps"], json!(MAX_THROTTLED_FPS));
        let top_hz = loop_set(world, &json!({ "fixed_hz": MAX_FIXED_HZ })).expect("max fixed hz");
        assert_eq!(top_hz["after"]["fixed_hz"], MAX_FIXED_HZ);
        let low = loop_set(
            world,
            &json!({
                "max_fps": MIN_PACING_FPS,
                "throttled_fps": MIN_THROTTLED_FPS,
                "max_substeps": 1,
                "fixed_hz": 1.0,
            }),
        )
        .expect("low endpoints");
        assert_eq!(low["after"]["max_fps"], json!(MIN_PACING_FPS));
        assert_eq!(low["after"]["throttled_fps"], json!(MIN_THROTTLED_FPS));
        assert_eq!(low["after"]["max_substeps"], 1);
        let kept = loop_get(world).expect("kept");
        for bad in [
            json!({ "mode": "warp" }),
            json!({ "mode": "run_frames" }),
            json!({ "max_fps": -1 }),
            json!({ "max_fps": 0.5 }),
            json!({ "max_fps": f64::from(MAX_PACING_FPS) + 1.0 }),
            json!({ "fixed_hz": 0 }),
            json!({ "fixed_hz": MAX_FIXED_HZ + 1.0 }),
            json!({ "max_substeps": 0 }),
            json!({ "max_substeps": MAX_SUBSTEPS_LIMIT + 1 }),
            json!({ "throttled_fps": "fast" }),
            json!({ "throttled_fps": 0.5 }),
            json!({ "throttled_fps": f64::from(MAX_THROTTLED_FPS) + 1.0 }),
        ] {
            assert!(loop_set(world, &bad).is_err(), "{bad}");
            assert_eq!(
                loop_get(world).expect("get"),
                kept,
                "errors change nothing: {bad}"
            );
        }
    }

    #[test]
    fn run_frames_freezes_through_the_protocol() {
        let mut app = App::new();
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(10)));
        loop_set(app.world_mut(), &json!({ "mode": "run_frames", "frames": 2 })).expect("set");
        for _ in 0..4 {
            app.update();
        }
        let state = loop_get(app.world_mut()).expect("get");
        assert_eq!(state["frozen"], true);
        assert_eq!(state["frames_in_mode"], 2);
        assert_eq!(
            app.world().resource::<Time>().elapsed(),
            Duration::from_millis(20)
        );
    }

    #[test]
    fn quit_request_follows_the_close_policy() {
        let mut app = App::new();
        app.add_message::<regia_platform::WindowCloseRequested>();
        let out = quit_request(app.world_mut(), &json!({})).expect("quit");
        assert_eq!(out["will_exit"], true);
        assert_eq!(
            app.should_exit(),
            Some(AppExit::Success),
            "headless: exits directly"
        );

        app.insert_resource(regia_platform::ClosePolicy {
            exit_on_primary_close: false,
            back_closes_primary: true,
        });
        let out = quit_request(app.world_mut(), &json!({})).expect("quit");
        assert_eq!(out["will_exit"], false);
        app.update();
        assert_eq!(app.should_exit(), None, "the game decides");
    }

    /// A host with `AgentPlugin` but no `PlatformPlugin` (the editor, headless tools) still has
    /// the close message registered, so `quit.request` delivers it instead of logging an error.
    #[test]
    fn quit_request_works_without_the_platform_plugin() {
        let mut app = App::new();
        app.add_plugins(crate::AgentPlugin);
        app.world_mut().spawn(regia_platform::PrimaryWindow);
        let out = quit_request(app.world_mut(), &json!({})).expect("quit");
        assert_eq!(out["will_exit"], true);
        assert_eq!(
            app.world()
                .resource::<Messages<regia_platform::WindowCloseRequested>>()
                .len(),
            1,
            "the close message was delivered to a registered queue"
        );
    }

    #[test]
    fn quit_request_fast_hides_now_and_flushes_a_frame_later() {
        let mut app = App::new();
        app.insert_resource(regia_platform::FastExitSettings {
            hard_deadline: None,
            ..regia_platform::FastExitSettings::default()
        });
        // ClosePolicy does not apply: the fast exit is the host's decision.
        app.insert_resource(regia_platform::ClosePolicy {
            exit_on_primary_close: false,
            back_closes_primary: true,
        });
        app.update();
        let out = quit_request(app.world_mut(), &json!({ "mode": "fast" })).expect("fast quit");
        assert_eq!(out["mode"], "fast");
        assert_eq!(out["will_exit"], true);
        assert_eq!(out["windows_hidden"], 0, "no OS window in this app");
        let requested = out["requested_frame"].as_u64().expect("requested_frame");
        assert_eq!(out["flush_frame"], json!(requested + FAST_QUIT_DELAY_FRAMES));
        assert!(regia_platform::fast_exit_requested(app.world()));
        assert!(!regia_platform::fast_exit_due(&mut app), "the reply gets a frame");
        app.update();
        assert!(regia_platform::fast_exit_due(&mut app));
    }

    #[test]
    fn quit_request_rejects_an_unknown_mode() {
        let mut app = App::new();
        for bad in [json!({ "mode": "teardown" }), json!({ "mode": 3 })] {
            let err = quit_request(app.world_mut(), &bad).expect_err("bad mode");
            assert_eq!(err.code, crate::protocol::INVALID_PARAMS, "{bad}");
        }
        assert!(!regia_platform::fast_exit_requested(app.world()));
        assert_eq!(app.should_exit(), None, "nothing requested");
        let out = quit_request(app.world_mut(), &json!({ "mode": "clean" })).expect("clean");
        assert_eq!(out["mode"], "clean");
    }

    #[test]
    fn coroutines_list_reports_waits() {
        let mut app = App::new();
        assert!(coroutines_list(app.world_mut()).is_err(), "no plugin: NOT_FOUND");
        app.add_plugins(regia_framework::coroutine::CoroutinePlugin);
        app.world_mut()
            .resource_mut::<Coroutines>()
            .start_labeled("wave", None, |co| async move { co.wait_frames(10).await });
        app.update();
        let out = coroutines_list(app.world_mut()).expect("list");
        assert_eq!(out["count"], 1);
        assert_eq!(out["coroutines"][0]["label"], "wave");
        assert_eq!(out["coroutines"][0]["waiting"], "frames");
        assert_eq!(out["coroutines"][0]["frames_left"], 10);
    }
}
