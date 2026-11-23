// SPDX-License-Identifier: MPL-2.0
//! Review proofs for `anim.locomotion_state`, `anim.gait_simulate` and
//! `anim.foot_plant_simulate` (ADR-8708). Each test is a concrete input the book
//! page says is refused or answered with finite numbers. A failure is the finding.

use regia_agent::{LocalReply, call_local};
use regia_animation::AnimationPlugin;
use regia_animation::foot_ik::FootPlantReport;
use regia_animation::gait::{GaitParams, GaitPatternSet, GaitPosture, GaitState, MAX_LEGS, leg_array};
use regia_app::prelude::{App, MinimalPlugins};
use serde_json::{Value, json};

fn app() -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, AnimationPlugin));
    app
}

fn reply(app: &mut App, method: &str, params: Value) -> Result<Value, String> {
    match call_local(app.world_mut(), method, params, Some("locomotion-review")) {
        LocalReply::Done(Ok(value)) => Ok(value),
        LocalReply::Done(Err(error)) => {
            let code = error
                .data
                .as_ref()
                .and_then(|data| data.get("code"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            Err(code)
        }
        reply => panic!("{method}: {reply:?}"),
    }
}

fn finite_number(value: &Value) -> bool {
    value.as_f64().is_some_and(|n| n.is_finite())
}

fn assert_finite_tree(value: &Value, path: &str) {
    match value {
        Value::Null => panic!("{path} is null; a pose or metric number must be finite"),
        Value::Number(_) => {
            assert!(finite_number(value), "{path} is not finite: {value}");
        }
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                assert_finite_tree(item, &format!("{path}[{i}]"));
            }
        }
        Value::Object(map) => {
            for (key, item) in map {
                // `attitude` is documented null until the attitude lane. `trace` is null
                // when no rows were asked for. `max_reach_ratio` and the stability margins
                // are null only when no planted foot existed.
                if key == "attitude" || key == "trace" {
                    continue;
                }
                assert_finite_tree(item, &format!("{path}.{key}"));
            }
        }
        Value::Bool(_) | Value::String(_) => {}
    }
}

fn hexapod() -> GaitParams {
    GaitParams {
        forward_speed: 2.4,
        reverse_speed: 1.1,
        strafe_speed: 0.8,
        acceleration: 3.0,
        step_length: 0.6,
        clearance: 0.7,
        swing_lift: 0.18,
        max_slope: 0.9,
        idle_speed: 0.02,
        max_yaw_rate: 0.8,
        hip_offsets: leg_array(
            [
                [-0.4, 0.5],
                [0.4, 0.5],
                [-0.5, 0.0],
                [0.5, 0.0],
                [-0.4, -0.5],
                [0.4, -0.5],
            ],
            [0.0; 2],
        ),
        settle_distance: 0.05,
        settle_period: 0.3,
        turn_step_distance: 0.15,
        turn_step_angle: 0.35,
        ground_band: 50.0,
        foothold_candidates: 0,
        foothold_spacing: 0.25,
        max_reach: [5.0; MAX_LEGS],
        patterns: GaitPatternSet::tripod(),
        hang_drop: 0.3,
        posture: GaitPosture::UPRIGHT,
    }
}

/// `ground.kind` must be one of the four names. A number is not a name.
#[test]
fn a_wrong_typed_ground_kind_is_bad_ground() {
    let mut app = app();
    let err = reply(
        &mut app,
        "anim.gait_simulate",
        json!({ "steps": 2, "ground": { "kind": 7, "height": 2.0 } }),
    )
    .expect_err("kind 7 is not flat, slope, steps or rubble");
    assert_eq!(err, "bad_ground", "got {err}");
}

/// A JSON number past the f32 range is not a foot-plant setting. The call must
/// refuse it. Echoing the field as null and running a fallback is not a setting.
#[test]
fn a_non_finite_foot_plant_setting_is_refused() {
    let mut app = app();
    let err = reply(
        &mut app,
        "anim.foot_plant_simulate",
        json!({ "steps": 4, "settings": { "max_tilt": 1e40 } }),
    )
    .expect_err("max_tilt 1e40 does not fit in a finite f32 setting");
    assert!(
        err == "F0002" || err == "gait_settings",
        "expected a parameter refusal, got {err}"
    );
}

/// `lock_max_drift` treats a non-finite value as unlimited drift, and the answer
/// then shows that field as null. The call must refuse the input.
#[test]
fn an_infinite_lock_drift_is_not_a_successful_null_setting() {
    let mut app = app();
    match reply(
        &mut app,
        "anim.foot_plant_simulate",
        json!({ "steps": 4, "settings": { "lock_max_drift": 1e40 } }),
    ) {
        Err(code) => assert!(
            code == "F0002" || code == "gait_settings",
            "expected a parameter refusal, got {code}"
        ),
        Ok(value) => {
            let shown = &value["settings"]["lock_max_drift"];
            panic!(
                "call succeeded; settings.lock_max_drift is {shown} (the run treats overflow as unlimited drift)"
            );
        }
    }
}

/// A finite `step_length` the stepper accepts must not come back as a null
/// `swing_bound`. Null is not in that metric's result.
#[test]
fn a_huge_finite_step_length_does_not_null_the_swing_bound() {
    let mut app = app();
    match reply(
        &mut app,
        "anim.gait_simulate",
        json!({
            "steps": 8,
            "inputs": [[0.0, 0.0, 0.0]],
            "params": { "step_length": 3.0e38 }
        }),
    ) {
        Err(code) => assert!(
            code == "gait_settings" || code == "gait_state" || code == "gait_step_too_large",
            "unexpected refusal {code}"
        ),
        Ok(value) => {
            let bound = &value["metrics"]["swing_bound"];
            assert!(
                finite_number(bound),
                "swing_bound must be a finite number, got {bound}; metrics {}",
                value["metrics"]
            );
            assert_finite_tree(&value["final"]["position"], "final.position");
        }
    }
}

/// A finite but tiny `max_reach` is still a reach. `null` means "no finite reach".
#[test]
fn a_tiny_finite_max_reach_does_not_report_a_null_ratio() {
    let mut app = app();
    match reply(
        &mut app,
        "anim.gait_simulate",
        json!({ "steps": 6, "params": { "max_reach": 1e-40 } }),
    ) {
        Err(code) => assert_eq!(code, "gait_settings", "tiny reach refused as {code}"),
        Ok(value) => {
            let ratio = &value["metrics"]["max_reach_ratio"];
            assert!(
                finite_number(ratio),
                "max_reach 1e-40 is finite, so max_reach_ratio must be a finite number, got {ratio}"
            );
        }
    }
}

/// A component loaded with a non-finite position (a scene field, or an agent write
/// of the reflected component) must not be published as a null pose coordinate.
#[test]
fn a_non_finite_gait_component_is_not_published_as_the_pose() {
    let mut app = app();
    let mut state = GaitState::at_rest(&hexapod(), [0.0, 0.7, 0.0], 0.0, |_, _| 0.0).expect("gait");
    state.position = [f32::NAN, 0.7, 0.0];
    state.feet[0].position = [f32::INFINITY, 0.0, 0.0];
    let rig = app.world_mut().spawn(state).id();
    match reply(
        &mut app,
        "anim.locomotion_state",
        json!({ "entity": rig.to_bits() }),
    ) {
        Err(code) => assert!(
            code == "gait_state" || code == "no_locomotion",
            "non-finite gait refused as {code}"
        ),
        Ok(value) => {
            assert_finite_tree(&value["gait"]["position"], "gait.position");
            assert_finite_tree(&value["gait"]["feet"], "gait.feet");
        }
    }
}

/// Same rule for a foot-plant report an agent or a file stored with a non-finite slide.
#[test]
fn a_non_finite_foot_plant_report_is_not_published() {
    let mut app = app();
    let report = FootPlantReport {
        weight: f32::NAN,
        max_slide: f32::INFINITY,
        ..FootPlantReport::default()
    };
    let rig = app.world_mut().spawn(report).id();
    match reply(
        &mut app,
        "anim.locomotion_state",
        json!({ "entity": rig.to_bits() }),
    ) {
        Err(code) => assert!(
            code == "gait_state" || code == "no_locomotion",
            "non-finite report refused as {code}"
        ),
        Ok(value) => {
            assert_finite_tree(&value["foot_plant"]["weight"], "foot_plant.weight");
            assert_finite_tree(&value["foot_plant"]["max_slide"], "foot_plant.max_slide");
        }
    }
}

/// Empty and zero-length controls are refused, or a degenerate polyline still
/// answers a finite pose. A panic or a null coordinate is a failure.
#[test]
fn zero_length_inputs_do_not_panic_or_publish_nan() {
    let mut app = app();
    for (params, code) in [
        (json!({ "inputs": [] }), "gait_input"),
        (json!({ "inputs": [[]] }), "gait_input"),
        (json!({ "inputs": [[1.0, 0.0, 0.0, 0]] }), "gait_input"),
        (
            json!({ "body_path": { "points": [], "speed": 1.0 } }),
            "gait_input",
        ),
        (json!({ "params": { "hip_offsets": [] } }), "gait_pattern"),
        (json!({ "steps": 0 }), "F0002"),
    ] {
        let err = reply(&mut app, "anim.gait_simulate", params.clone()).expect_err("refused");
        assert_eq!(err, code, "{params}");
    }
    let pose = reply(
        &mut app,
        "anim.gait_simulate",
        json!({
            "steps": 4,
            "body_path": { "points": [[0.0, 0.0], [0.0, 0.0]], "speed": 1.0 }
        }),
    )
    .expect("a zero-length polyline is a stopped body, not a panic");
    assert_finite_tree(&pose["final"], "final");
    assert_finite_tree(&pose["metrics"]["max_foot_jump"], "metrics.max_foot_jump");
    assert_finite_tree(&pose["metrics"]["swing_bound"], "metrics.swing_bound");
}

/// The same two seconds of walk at 30, 60 and 144 Hz stay finite, inside the
/// swing bound, and within 15 cm of each other at the end.
#[test]
fn gait_and_foot_plant_agree_at_30_60_and_144_hz() {
    let mut app = app();
    let mut ends = Vec::new();
    for hz in [30.0_f64, 60.0, 144.0] {
        let dt = 1.0 / hz;
        let steps = (2.0 / dt).round() as u64;
        let gait = reply(
            &mut app,
            "anim.gait_simulate",
            json!({
                "steps": steps,
                "dt": dt,
                "inputs": [[1.0, 0.0, 0.0]]
            }),
        )
        .unwrap_or_else(|code| panic!("{hz} Hz gait refused as {code}"));
        let metrics = &gait["metrics"];
        assert_finite_tree(metrics, "metrics");
        let jump = metrics["max_foot_jump"].as_f64().expect("jump");
        let bound = metrics["swing_bound"].as_f64().expect("bound");
        assert!(
            jump <= bound + 1e-4,
            "{hz} Hz foot jump {jump} > swing_bound {bound}"
        );
        let position = gait["final"]["position"].as_array().expect("position");
        ends.push((position[0].as_f64().expect("x"), position[2].as_f64().expect("z")));

        let plant = reply(
            &mut app,
            "anim.foot_plant_simulate",
            json!({ "steps": steps, "dt": dt }),
        )
        .unwrap_or_else(|code| panic!("{hz} Hz foot plant refused as {code}"));
        assert_finite_tree(&plant["metrics"], "plant.metrics");
        let slide = plant["metrics"]["max_slide"].as_f64().expect("slide");
        let stances = plant["metrics"]["stances"].as_u64().expect("stances");
        assert!(
            stances >= 1,
            "{hz} Hz measured no stances over 2 s: {}",
            plant["metrics"]
        );
        assert!(slide <= 0.01, "{hz} Hz max_slide {slide} over 2 s");
    }
    for (i, end) in ends.iter().enumerate().skip(1) {
        let dx = end.0 - ends[0].0;
        let dz = end.1 - ends[0].1;
        let gap = (dx * dx + dz * dz).sqrt();
        assert!(
            gap <= 0.15,
            "final XZ at rate {i} is {gap} m from the 30 Hz pose ({ends:?})"
        );
    }
}

fn repo_text(rel: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()))
}

/// The four locomotion methods are in the agent-protocol snapshot the gate checks.
#[test]
fn agent_protocol_snapshot_names_the_locomotion_methods() {
    let text = repo_text("docs/api-snapshots/agent-protocol.txt");
    for name in [
        "anim.gait_simulate",
        "anim.foot_plant_simulate",
        "anim.ik_report",
        "anim.locomotion_state",
    ] {
        assert!(
            text.contains(&format!("\nmethod: {name}\n")),
            "docs/api-snapshots/agent-protocol.txt has no `method: {name}`"
        );
    }
}

/// `GraphDebug.ik` is a public field. The animation API snapshot still lists the old struct.
#[test]
fn animation_api_snapshot_graph_debug_lists_ik() {
    let text = repo_text("docs/api-snapshots/regia_animation.txt");
    let line = text
        .lines()
        .find(|line| line.starts_with("debug::GraphDebug\t"))
        .expect("GraphDebug row");
    assert!(
        line.contains("ik:"),
        "GraphDebug snapshot has no ik field: {line}"
    );
}

/// ADR-8610 is both counted textures and gait ground. `cargo xtask adr-index` rejects that.
#[test]
fn adr_8610_is_not_duplicated() {
    let text = repo_text("docs/decisions.md");
    let headings = text.matches("## ADR-8610:").count();
    assert_eq!(headings, 1, "docs/decisions.md has {headings} ADR-8610 headings");
}

/// A missing, `null` or wrong-typed `body_path.points` / `body_path.speed` keeps the page's
/// `gait_input` code and names the field and its type like the typed readers do (ADR-2622,
/// ADR-10129): `data.param` and `data.expected`.
#[test]
fn body_path_refusals_name_the_field_and_its_type() {
    let mut app = app();
    let points = json!([[0.0, 0.0], [0.0, -1.0]]);
    for (body_path, param) in [
        (json!({ "points": "x", "speed": 1.0 }), "body_path.points"),
        (json!({ "points": null, "speed": 1.0 }), "body_path.points"),
        (json!({ "speed": 1.0 }), "body_path.points"),
        (json!({ "points": points, "speed": "fast" }), "body_path.speed"),
        (json!({ "points": points, "speed": null }), "body_path.speed"),
        (json!({ "points": points }), "body_path.speed"),
    ] {
        let params = json!({ "steps": 2, "body_path": body_path });
        let error = match call_local(
            app.world_mut(),
            "anim.gait_simulate",
            params.clone(),
            Some("locomotion-review"),
        ) {
            LocalReply::Done(Err(error)) => error,
            other => panic!("{params}: expected a refusal, got {other:?}"),
        };
        let data = error.data.unwrap_or_default();
        assert_eq!(data["code"], "gait_input", "{params}");
        assert_eq!(data["param"], param, "{params}: {data}");
        assert!(
            data["expected"].as_str().is_some_and(|e| !e.is_empty()),
            "{params}: no data.expected in {data}"
        );
    }
}
