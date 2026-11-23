// SPDX-License-Identifier: MPL-2.0
//! `detsim.hover_*` through the protocol dispatcher: every method and every error code.

#![cfg(test)]

use regia_app::prelude::*;
use regia_framework::det_hover::DetHoverPlugin;

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
    reply_rx.try_recv().expect("dispatcher answered")
}

fn ok(world: &mut World, method: &str, params: Value) -> Value {
    let response = call(world, method, params);
    assert!(response.error.is_none(), "{method}: {:?}", response.error);
    response.result.expect("a result")
}

/// The Regia code of a failed call (`data.code`).
fn code(world: &mut World, method: &str, params: Value) -> String {
    let response = call(world, method, params);
    let error = response
        .error
        .unwrap_or_else(|| panic!("{method} succeeded: {:?}", response.result));
    error
        .data
        .as_ref()
        .and_then(|d| d.get("code"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(DetHoverPlugin)
        .insert_resource(AgentPermissions {
            config: AgentConfig {
                rate_limit_per_second: 10_000,
                heavy_rate_limit_per_second: 10_000,
                ..AgentConfig::default()
            },
            source: None,
        })
        .init_resource::<crate::safety::AuditLog>()
        .init_resource::<crate::safety::AgentSession>();
    enable_fixed_step(&mut app, 60.0);
    app
}

#[test]
fn hover_step_moves_a_hull_and_continues_from_its_state() {
    let mut app = app();
    let world = app.world_mut();
    let full = ok(
        world,
        "detsim.hover_step",
        json!({ "input": { "throttle": 1.0 }, "steps": 120 }),
    );
    assert_eq!(full["report"]["steps"], 120);
    assert!(full["state"]["pos"][2].as_f64().unwrap_or(0.0) > 5.0, "{full}");
    // Two calls of 60 steps, the second from the first's state, equal one call of 120.
    let half = ok(
        world,
        "detsim.hover_step",
        json!({ "input": { "throttle": 1.0 }, "steps": 60 }),
    );
    let rest = ok(
        world,
        "detsim.hover_step",
        json!({ "input": { "throttle": 1.0 }, "steps": 60, "state": half["state"].clone() }),
    );
    assert_eq!(
        rest["digest"], full["digest"],
        "a returned state resumes bit for bit"
    );
    // The same call twice gives the same digest.
    let again = ok(
        world,
        "detsim.hover_step",
        json!({ "input": { "throttle": 1.0 }, "steps": 120 }),
    );
    assert_eq!(again["digest"], full["digest"]);
}

#[test]
fn hover_step_reads_params_world_and_floor_profile() {
    let mut app = app();
    let world = app.world_mut();
    let grid = json!({
        "origin": [-50.0, -50.0], "cell": 10.0, "cols": 11, "rows": 11,
        "heights": (0..121).map(|i| f64::from(i % 11) * 0.5).collect::<Vec<_>>(),
    });
    let r = ok(
        world,
        "detsim.hover_step",
        json!({
            "params": { "ride_height": 1.0 },
            "limits": { "hop_apex_m": 1.5 },
            "world": { "gravity_m_s2": 3.7, "low_g": "physical" },
            "flight": "clamped",
            "half_length": 3.0,
            "half_width": 1.5,
            "input": { "throttle": 0.5, "steer": 0.3, "hop": true },
            "floor_profile": grid,
            "dt": 0.02,
            "steps": 200,
        }),
    );
    assert!(r["report"]["hops"].as_u64().unwrap_or(0) >= 1, "{r}");
    assert_eq!(r["report"]["dt"].as_f64().map(|d| d as f32), Some(0.02));
    assert!(r["pose"]["position"][1].is_number());
}

#[test]
fn hover_probe_answers_every_script_and_is_stable() {
    let mut app = app();
    let world = app.world_mut();
    for script in ["accel", "brake", "turn", "hop", "crest"] {
        let a = ok(world, "detsim.hover_probe", json!({ "script": script }));
        let b = ok(world, "detsim.hover_probe", json!({ "script": script }));
        assert_eq!(a, b, "{script}");
        assert_eq!(a["script"], script);
        assert!(a["ticks"].as_u64().unwrap_or(0) > 0);
    }
    let accel = ok(world, "detsim.hover_probe", json!({ "script": "accel" }));
    assert!(accel["numbers"]["top_speed_m_s"].as_f64().unwrap_or(0.0) > 5.0);
    assert!(accel["numbers"]["time_to_90_s"].is_number());
}

#[test]
fn stateful_methods_list_get_and_set_input() {
    let mut app = app();
    let hull = app
        .world_mut()
        .spawn((
            DetHoverHull::DEFAULT,
            DetHoverId(5),
            DetHoverState::at_rest([0.0; 3], 0.0, &HoverParams::DEFAULT_HULL),
        ))
        .id();
    app.world_mut().spawn((DetHoverHull::DEFAULT, DetHoverId(2)));
    let list = ok(app.world_mut(), "detsim.hover_list", json!({}));
    let ids: Vec<u64> = list["hulls"]
        .as_array()
        .map(|a| a.iter().filter_map(|h| h["id"].as_u64()).collect())
        .unwrap_or_default();
    assert_eq!(ids, [2, 5], "listed in step order");
    assert_eq!(list["plugin"], true);

    let set = ok(
        app.world_mut(),
        "detsim.hover_set_input",
        json!({ "entity": hull.to_bits(), "input": { "throttle": 1.0 } }),
    );
    assert_eq!(set["input"]["throttle"].as_f64(), Some(1.0));
    for _ in 0..60 {
        app.update();
    }
    let got = ok(
        app.world_mut(),
        "detsim.hover_get",
        json!({ "entity": hull.to_bits() }),
    );
    assert_eq!(got["id"], 5);
    assert_eq!(got["hull"]["flight"], "air_time");
    assert!(
        got["state"]["pos"][2].as_f64().unwrap_or(0.0) > 1.0,
        "the set input drove the hull: {got}"
    );
    assert!(got["pose"]["lean_pitch"].is_number());
}

#[test]
fn every_error_code_is_reachable() {
    let mut app = app();
    let plain = app.world_mut().spawn_empty().id();
    let world = app.world_mut();
    let step = "detsim.hover_step";
    let input = json!({ "throttle": 1.0 });
    for (params, want) in [
        (
            json!({ "input": input, "params": { "ride_height": -1.0 } }),
            "hover_bad_params",
        ),
        (
            json!({ "input": input, "params": { "no_such_field": 1.0 } }),
            "hover_bad_params",
        ),
        (
            json!({ "input": input, "limits": { "hop_apex_m": 1.0e39 } }),
            "hover_bad_params",
        ),
        (
            json!({ "input": input, "world": { "gravity_m_s2": 0.0 } }),
            "hover_bad_params",
        ),
        (json!({ "input": input, "flight": "fast" }), "hover_bad_params"),
        (
            json!({ "input": input, "half_length": 500.0 }),
            "hover_bad_params",
        ),
        (json!({}), "hover_bad_input"),
        (json!({ "input": { "throttle": 1.0e39 } }), "hover_bad_input"),
        (json!({ "input": { "boost": 1.0 } }), "hover_bad_input"),
        (
            json!({ "input": input, "floor": 1.0, "floor_profile": { "cell": 1.0 } }),
            "hover_bad_input",
        ),
        (
            json!({ "input": input, "floor_profile": { "cell": 1.0, "cols": 1, "rows": 1, "heights": [0.0] } }),
            "hover_bad_input",
        ),
        // A wrong-typed count and one past u32 are refused like a missing one (ADR-2622 ratchet fix).
        (
            json!({ "input": input, "floor_profile": { "cell": 1.0, "cols": "2", "rows": 2, "heights": [0.0, 0.0, 0.0, 0.0] } }),
            "hover_bad_input",
        ),
        (
            json!({ "input": input, "floor_profile": { "cell": 1.0, "cols": 2, "rows": 4_294_967_298_u64, "heights": [0.0, 0.0, 0.0, 0.0] } }),
            "hover_bad_input",
        ),
        (json!({ "input": input, "dt": 0.5 }), "hover_bad_input"),
        (
            json!({ "input": input, "state": { "pos": [0.0, 1.0e39, 0.0] } }),
            "hover_bad_input",
        ),
        (json!({ "input": input, "steps": 0 }), "hover_steps_limit"),
        (json!({ "input": input, "steps": 100_001 }), "hover_steps_limit"),
    ] {
        assert_eq!(code(world, step, params.clone()), want, "{params}");
    }
    let field = call(
        world,
        step,
        json!({ "input": input, "params": { "ride_height": -1.0 } }),
    );
    let data = field.error.and_then(|e| e.data).unwrap_or(Value::Null);
    assert_eq!(data["field"], "ride_height");
    assert_eq!(data["reason"], "negative");

    assert_eq!(
        code(world, "detsim.hover_probe", json!({ "script": "loop" })),
        "hover_bad_input"
    );
    assert_eq!(
        code(
            world,
            "detsim.hover_probe",
            json!({ "script": "accel", "params": { "max_forward_speed": f64::from(f32::MAX) * 2.0 } })
        ),
        "hover_bad_params"
    );
    assert_eq!(
        code(world, "detsim.hover_get", json!({ "entity": plain.to_bits() })),
        "hover_no_hull"
    );
    assert_eq!(
        code(
            world,
            "detsim.hover_set_input",
            json!({ "entity": plain.to_bits(), "input": {} })
        ),
        "hover_no_hull"
    );
    assert_eq!(
        code(
            world,
            "detsim.hover_set_input",
            json!({ "entity": plain.to_bits(), "input": { "hop": 1 } })
        ),
        "hover_bad_input"
    );
}

#[test]
fn hover_step_of_the_default_hull_matches_tick_hull() {
    let mut app = app();
    let r = ok(
        app.world_mut(),
        "detsim.hover_step",
        json!({ "input": { "throttle": 1.0, "steer": 0.5 }, "steps": 300 }),
    );
    let hull = DetHoverHull::DEFAULT;
    let mut body = HoverBody::at_rest([0.0; 3], 0.0, &hull.params);
    let input = DetHoverInput::from_axes([1.0, 0.0, 0.5]);
    for _ in 0..300 {
        tick_hull(
            &hull,
            &mut body,
            &input,
            &FlatFloor(0.0),
            &WorldPhysics::EARTH,
            0.0,
            tick_dt(),
        );
    }
    assert_eq!(r["digest"].as_str(), Some(digest(&body).as_str()));
}
