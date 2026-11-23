// SPDX-License-Identifier: MPL-2.0
//! Frame caps per app state and the dynamic-resolution hold on the agent protocol (ADR-7889,
//! book `agents/frame-pacing.md`): `app.pacing` reads the [`PacingState`], the [`PacingCaps`],
//! the player's cap, the cap that paces and the measured frame rate; `app.set_pacing_state` sets
//! the state and the caps; `render.dynres` reads the app's [`DynResGovernor`], `held` included.

use regia_app::prelude::*;
use regia_app::{Diagnostics, MAX_PACING_FPS, MIN_PACING_FPS, PacingCaps, PacingState};
use regia_render::DynResGovernor;
use serde_json::{Map, Value, json};

use crate::protocol::{INVALID_PARAMS, RpcError};

/// Error code of an unknown `state` name.
pub(crate) const UNKNOWN_STATE: &str = "pacing_unknown_state";
/// Error code of a cap that is not `null` or a number in `1..=10000`.
pub(crate) const BAD_CAP: &str = "pacing_bad_cap";

fn caps_json(caps: &PacingCaps) -> Value {
    let mut out = Map::new();
    for state in PacingState::ALL {
        out.insert(state.name().into(), json!(caps.cap(state)));
    }
    Value::Object(out)
}

fn pacing_json(world: &World) -> Value {
    let state = world.get_resource::<PacingState>().copied().unwrap_or_default();
    let caps = world.get_resource::<PacingCaps>().copied().unwrap_or_default();
    let pacing = world.get_resource::<FramePacing>();
    let player = pacing.and_then(|p| p.target_fps);
    let state_fps = caps.cap(state);
    // From the resources, not the copy the app makes in `Last`, so a change made this frame reads
    // back at once.
    let effective = regia_app::tighter_cap(player, state_fps);
    let control = world.get_resource::<LoopControl>();
    let paces = control.is_none_or(LoopControl::paces);
    let measured = world
        .get_resource::<Diagnostics>()
        .map(|d| d.fps)
        .filter(|fps| fps.is_finite() && *fps > 0.0);
    json!({
        "state": state.name(),
        "caps": caps_json(&caps),
        "player_fps": player,
        "state_fps": state_fps,
        "effective_fps": effective,
        "period_ms": effective.map(|fps| 1000.0 / f64::from(fps)),
        "paces": paces,
        "loop_mode": control.map_or("continuous", |c| c.mode().name()),
        "measured_fps": measured,
    })
}

/// `app.pacing`.
pub(crate) fn app_pacing(world: &mut World) -> Result<Value, RpcError> {
    Ok(pacing_json(world))
}

fn bad_state(name: &str) -> RpcError {
    RpcError::new(
        INVALID_PARAMS,
        UNKNOWN_STATE,
        format!("[{UNKNOWN_STATE}] unknown pacing state `{name}`"),
        "state is play, paused, menu or loading; see book/agents/frame-pacing.md",
    )
}

fn bad_cap(state: &str) -> RpcError {
    RpcError::new(
        INVALID_PARAMS,
        BAD_CAP,
        format!("[{BAD_CAP}] caps.{state} must be null or a number of frames per second in 1..=10000"),
        "send null to clear a cap; see book/agents/frame-pacing.md",
    )
}

/// Reads `caps` into a copy of `current`: each named state takes `null` (no cap) or a number in
/// [`MIN_PACING_FPS`]`..=`[`MAX_PACING_FPS`]; unnamed states keep their cap.
fn parse_caps(current: PacingCaps, caps: &Value) -> Result<PacingCaps, RpcError> {
    let Value::Object(caps) = caps else {
        return Err(RpcError::wrong_type("caps", "an object"));
    };
    let mut out = current;
    for (name, value) in caps {
        let Some(state) = PacingState::from_name(name) else {
            return Err(bad_state(name));
        };
        let fps = match value {
            Value::Null => None,
            Value::Number(n) => {
                let fps = n.as_f64().ok_or_else(|| bad_cap(name))?;
                if !(f64::from(MIN_PACING_FPS)..=f64::from(MAX_PACING_FPS)).contains(&fps) {
                    return Err(bad_cap(name));
                }
                Some(fps as f32)
            }
            _ => return Err(bad_cap(name)),
        };
        out.set(state, fps);
    }
    Ok(out)
}

/// `app.set_pacing_state`: `{ state?, caps? }` (at least one). Validates everything before it
/// changes anything; answers `{ before, after }`, two `app.pacing` answers.
pub(crate) fn app_set_pacing_state(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let state = match params.get("state") {
        None | Some(Value::Null) => None,
        Some(Value::String(name)) => Some(PacingState::from_name(name).ok_or_else(|| bad_state(name))?),
        Some(_) => return Err(RpcError::wrong_type("state", "a string")),
    };
    let current = world.get_resource::<PacingCaps>().copied().unwrap_or_default();
    let caps = match params.get("caps") {
        None | Some(Value::Null) => None,
        Some(caps) => Some(parse_caps(current, caps)?),
    };
    if state.is_none() && caps.is_none() {
        return Err(RpcError::invalid_params("pass `state`, `caps` or both"));
    }
    let before = pacing_json(world);
    if let Some(state) = state {
        world.insert_resource(state);
    }
    if let Some(caps) = caps {
        world.insert_resource(caps);
    }
    Ok(json!({ "before": before, "after": pacing_json(world) }))
}

/// `render.dynres`: the app's [`DynResGovernor`]; `present: false` when the host installed none.
pub(crate) fn render_dynres(world: &mut World) -> Result<Value, RpcError> {
    let Some(governor) = world.get_resource::<DynResGovernor>() else {
        return Ok(json!({ "present": false }));
    };
    let controller = &governor.controller;
    let config = controller.config();
    Ok(json!({
        "present": true,
        "permille": controller.permille(),
        "held": controller.is_held(),
        "hold_outside_play": governor.hold_outside_play,
        "bound": controller.bound().label(),
        "median_gpu_ms": controller.median_gpu_ms(),
        "budget_ms": config.budget_ms,
        "min_permille": config.min_permille,
        "max_permille": config.max_permille,
    }))
}

#[cfg(test)]
mod tests {
    use regia_render::{DynResConfig, DynResController};

    use super::*;
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
        // Dispatch is synchronous; no clock or sleeping is needed to observe its reply.
        reply_rx.try_recv().expect("dispatcher answered")
    }

    fn ok(reply: Response) -> Value {
        assert!(reply.error.is_none(), "{:?}", reply.error);
        reply.result.expect("result")
    }

    fn code(reply: &Response) -> Option<String> {
        reply
            .error
            .as_ref()
            .and_then(|e| e.data.as_ref())
            .and_then(|d| d["code"].as_str())
            .map(str::to_owned)
    }

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .insert_resource(AgentPermissions {
                config: AgentConfig {
                    rate_limit_per_second: 10_000,
                    ..AgentConfig::default()
                },
                source: None,
            })
            .init_resource::<crate::safety::AuditLog>()
            .init_resource::<crate::safety::AgentSession>();
        app
    }

    #[test]
    fn set_pacing_state_caps_the_menu_through_the_dispatcher() {
        let mut app = app();
        app.world_mut().resource_mut::<FramePacing>().target_fps = Some(144.0);
        let got = ok(call(app.world_mut(), "app.pacing", json!({})));
        assert_eq!(got["state"], "play");
        assert_eq!(got["caps"]["menu"], Value::Null);
        assert_eq!(got["effective_fps"], 144.0);
        let got = ok(call(
            app.world_mut(),
            "app.set_pacing_state",
            json!({ "state": "menu", "caps": { "menu": 60, "paused": 30 } }),
        ));
        assert_eq!(got["before"]["state"], "play");
        assert_eq!(got["after"]["state"], "menu");
        assert_eq!(got["after"]["caps"]["menu"], 60.0);
        assert_eq!(got["after"]["effective_fps"], 60.0);
        assert_eq!(got["after"]["player_fps"], 144.0);
        // The runner's pacing follows on the next frame.
        app.update();
        assert_eq!(app.world().resource::<FramePacing>().effective_fps(), Some(60.0));
        let got = ok(call(
            app.world_mut(),
            "app.set_pacing_state",
            json!({ "state": "paused" }),
        ));
        assert_eq!(got["after"]["effective_fps"], 30.0);
        assert_eq!(got["after"]["caps"]["menu"], 60.0, "unnamed caps are kept");
        let got = ok(call(
            app.world_mut(),
            "app.set_pacing_state",
            json!({ "state": "play", "caps": { "paused": null } }),
        ));
        assert_eq!(got["after"]["caps"]["paused"], Value::Null);
        assert_eq!(got["after"]["effective_fps"], 144.0);
    }

    #[test]
    fn set_pacing_state_refuses_nonsense_and_changes_nothing() {
        let mut app = app();
        for (params, want) in [
            (json!({ "state": "lobby" }), Some(UNKNOWN_STATE)),
            (json!({ "caps": { "lobby": 30 } }), Some(UNKNOWN_STATE)),
            (json!({ "state": "menu", "caps": { "menu": 0 } }), Some(BAD_CAP)),
            (json!({ "caps": { "menu": 20000 } }), Some(BAD_CAP)),
            (json!({ "caps": { "menu": "60" } }), Some(BAD_CAP)),
            (json!({ "caps": 60 }), Some("F0002")),
            (json!({ "state": 3 }), Some("F0002")),
            (json!({}), Some("F0002")),
        ] {
            let reply = call(app.world_mut(), "app.set_pacing_state", params.clone());
            assert_eq!(code(&reply).as_deref(), want, "{params}");
        }
        assert!(
            app.world().get_resource::<PacingState>().is_none(),
            "nothing inserted"
        );
        assert!(
            app.world().get_resource::<PacingCaps>().is_none(),
            "nothing inserted"
        );
    }

    #[test]
    fn dynres_reports_absent_then_held_outside_play() {
        let mut app = app();
        let got = ok(call(app.world_mut(), "render.dynres", json!({})));
        assert_eq!(got, json!({ "present": false }));
        app.insert_resource(DynResGovernor::new(DynResController::new(
            DynResConfig::default(),
            900,
        )));
        let got = ok(call(app.world_mut(), "render.dynres", json!({})));
        assert_eq!(got["present"], true);
        assert_eq!(got["permille"], 900);
        assert_eq!(got["held"], false);
        let mut governor = app.world_mut().resource_mut::<DynResGovernor>();
        let _ = governor.feed(PacingState::Menu, 3.0, 16.7);
        let got = ok(call(app.world_mut(), "render.dynres", json!({})));
        assert_eq!(got["held"], true);
        assert_eq!(got["median_gpu_ms"], 0.0, "a held frame records nothing");
    }
}
