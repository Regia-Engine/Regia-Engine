// SPDX-License-Identifier: MPL-2.0
//! Locomotion quality through the dispatcher (ADR-8708): `anim.locomotion_state`,
//! `anim.gait_simulate`, `anim.foot_plant_simulate` and `anim.ik_report`, one end-to-end call
//! per method and per error code (`book/agents/locomotion-quality.md`).

use std::sync::Arc;

use regia_agent::{AgentSettings, LocalReply, call_local};
use regia_animation::chain_ik::ChainSolver;
use regia_animation::clip::AnimationClip;
use regia_animation::components::spawn_skeleton;
use regia_animation::eval::ClipLibrary;
use regia_animation::foot_ik::{FootPlacementStatus, FootPlantReport};
use regia_animation::gait::{
    GaitParams, GaitPatternSet, GaitPosture, GaitState, MAX_LEGS, leg_array, step_gait,
};
use regia_animation::graph::{
    AnimGraph, AnimNode, ChainIKNode, ClipNode, IkTarget, LookAtNode, TwoBoneIKNode,
};
use regia_animation::ik_lod::IkLod;
use regia_animation::params::{ParamDecl, ParamValue};
use regia_animation::skeleton::LocalBind;
use regia_animation::{AnimationPlugin, Animator, Skeleton};
use regia_app::prelude::{App, Entity, MinimalPlugins};
use regia_core::math::Vec3;
use regia_scene::Transform;
use serde_json::{Value, json};

fn app() -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, AnimationPlugin));
    app
}

fn call(app: &mut App, method: &str, params: Value) -> Value {
    match call_local(app.world_mut(), method, params, Some("locomotion-test")) {
        LocalReply::Done(Ok(value)) => value,
        reply => panic!("{method}: {reply:?}"),
    }
}

/// The `data.code` of a refused call.
fn refused(app: &mut App, method: &str, params: Value) -> String {
    match call_local(app.world_mut(), method, params, Some("locomotion-test")) {
        LocalReply::Done(Err(error)) => {
            let data = error.data.as_ref().expect("error data");
            assert!(data.get("hint").and_then(Value::as_str).is_some(), "{error:?}");
            data["code"].as_str().expect("code").to_string()
        }
        reply => panic!("{method} was not refused: {reply:?}"),
    }
}

fn f(value: &Value) -> f64 {
    value.as_f64().unwrap_or_else(|| panic!("not a number: {value}"))
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

// --- anim.locomotion_state -------------------------------------------------------------------

#[test]
fn locomotion_state_reports_gait_foot_plant_and_status() {
    let params = hexapod();
    let mut state = GaitState::at_rest(&params, [0.0, 0.7, 0.0], 0.0, |_, _| 0.0).expect("gait");
    for _ in 0..30 {
        step_gait(&params, &mut state, [1.0, 0.0, 0.0], 1.0 / 60.0, |_, _| 0.0).expect("step");
    }
    let mut app = app();
    let report = FootPlantReport {
        weight: 1.0,
        planted: vec![1.0, 0.0],
        max_slide: 0.004,
        ..FootPlantReport::default()
    };
    let status = FootPlacementStatus {
        probe_installed: true,
        legs_resolved: true,
        ..FootPlacementStatus::default()
    };
    let rig = app.world_mut().spawn((state, report, status)).id();
    let out = call(
        &mut app,
        "anim.locomotion_state",
        json!({ "entity": rig.to_bits() }),
    );
    let gait = &out["gait"];
    assert_eq!(gait["mode"], "integrating");
    assert_eq!(gait["legs"], 6);
    assert_eq!(gait["feet"].as_array().expect("feet").len(), 6);
    assert_eq!(gait["pattern"]["duty_factor"], json!(0.5));
    assert_eq!(gait["refused"], 0);
    assert_eq!(gait["forced_steps"], 0);
    assert_eq!(gait["replants"], 0);
    assert!(gait["stability_margin"].is_number(), "{gait}");
    // Body posture (ADR-8706) through the roll-up (ADR-8737): an upright walker on level
    // ground reports no tilt, exactly.
    let attitude = &gait["attitude"];
    assert_eq!(attitude["pitch"], json!(0.0), "{gait}");
    assert_eq!(attitude["roll"], json!(0.0), "{gait}");
    assert_eq!(attitude["quat"], json!([0.0, 0.0, 0.0, 1.0]), "{gait}");
    assert!(attitude["body_height_target"].is_number(), "{gait}");
    let planted = gait["feet"]
        .as_array()
        .expect("feet")
        .iter()
        .filter(|foot| foot["planted"] == true)
        .count();
    assert_eq!(gait["planted_count"], planted);
    assert_eq!(out["foot_plant"]["max_slide"], json!(0.004_f32));
    assert_eq!(out["foot_plant"]["planted"], json!([1.0, 0.0]));
    assert_eq!(out["foot_placement"]["probe_installed"], true);
    assert_eq!(
        out["foot_placement"]["lod"], "full",
        "IK LOD (ADR-8728) is reported"
    );
    // A rig with only a foot-plant report answers that part.
    let foot_only = app.world_mut().spawn(FootPlantReport::default()).id();
    let out = call(
        &mut app,
        "anim.locomotion_state",
        json!({ "entity": foot_only.to_bits() }),
    );
    assert!(out["gait"].is_null() && out["foot_placement"].is_null());
    assert_eq!(out["foot_plant"]["weight"], json!(0.0));
}

#[test]
fn locomotion_state_refuses_missing_entities_and_rigs_without_locomotion() {
    let mut app = app();
    let gone = app.world_mut().spawn_empty().id();
    assert!(app.world_mut().despawn(gone));
    assert_eq!(
        refused(
            &mut app,
            "anim.locomotion_state",
            json!({ "entity": gone.to_bits() })
        ),
        "no_such_entity"
    );
    let plain = app.world_mut().spawn(Transform::default()).id();
    assert_eq!(
        refused(
            &mut app,
            "anim.locomotion_state",
            json!({ "entity": plain.to_bits() })
        ),
        "no_locomotion"
    );
}

// --- anim.gait_simulate ----------------------------------------------------------------------

#[test]
fn gait_simulate_is_deterministic() {
    let mut app = app();
    for params in [
        json!({ "steps": 900, "ground": { "kind": "rubble", "amplitude": 0.12, "cell": 0.35, "seed": 7 },
                "inputs": [[1.0, 0.0, 0.2, 400], [0.5, 0.3, -0.4]], "trace_every": 100 }),
        json!({ "steps": 600, "ground": { "kind": "slope", "rise_z": -0.2 },
                "body_path": { "points": [[0.0, 0.0], [0.0, -3.0], [2.0, -5.0]], "speed": 1.2 } }),
        json!({ "steps": 600, "ground": { "kind": "steps", "rise": 0.1, "run": 0.5, "axis": "z" },
                "params": { "hip_offsets": [[-0.3, 0.5], [0.3, 0.5], [-0.3, -0.5], [0.3, -0.5]],
                            "patterns": ["quadruped_walk", "trot"], "switch_speeds": [1.0], "hysteresis": 0.1 },
                "inputs": [[1.0, 0.0, 0.0]] }),
    ] {
        let once = call(&mut app, "anim.gait_simulate", params.clone());
        let twice = call(&mut app, "anim.gait_simulate", params);
        assert_eq!(once, twice, "same inputs, same answer");
        let hash = once["metrics"]["final_pose_hash"].as_str().expect("hash");
        assert_eq!(hash.len(), 16, "{hash}");
        assert!(f(&once["metrics"]["max_foot_jump"]) > 0.0, "{once}");
    }
    // Another seed moves the result.
    let a = call(
        &mut app,
        "anim.gait_simulate",
        json!({ "steps": 300, "ground": { "kind": "rubble", "seed": 1 } }),
    );
    let b = call(
        &mut app,
        "anim.gait_simulate",
        json!({ "steps": 300, "ground": { "kind": "rubble", "seed": 2 } }),
    );
    assert_ne!(a["metrics"]["final_pose_hash"], b["metrics"]["final_pose_hash"]);
    let traced = call(
        &mut app,
        "anim.gait_simulate",
        json!({ "steps": 300, "trace_every": 100 }),
    );
    assert_eq!(traced["trace"].as_array().expect("trace").len(), 3);
    assert_eq!(traced["trace"][2]["step"], 300);
}

/// Walk, stop, turn in place, rest: the feet settle and turn by stepping (ADR-8539), so no
/// planted foot moves and no foot moves faster than a swing allows. The control run drives a
/// body that jumps farther than a step every frame: replants move planted feet, and the
/// metrics show it.
#[test]
fn gait_simulate_stop_and_turn_has_no_foot_jump() {
    let mut app = app();
    let out = call(
        &mut app,
        "anim.gait_simulate",
        json!({
            "steps": 420,
            "inputs": [[0.5, 0.0, 0.0, 120], [0.0, 0.0, 0.0, 60], [0.0, 0.0, 1.0, 120], [0.0, 0.0, 0.0, 120]],
        }),
    );
    let m = &out["metrics"];
    let (jump, bound) = (f(&m["max_foot_jump"]), f(&m["swing_bound"]));
    println!(
        "stop_and_turn max_foot_jump={jump:.4} swing_bound={bound:.4} slip={} min_planted={}",
        m["max_planted_slip"], m["min_planted"]
    );
    assert!(jump <= bound, "a foot jumped: {m}");
    assert_eq!(f(&m["max_planted_slip"]), 0.0, "a planted foot moved: {m}");
    assert_eq!(m["replants"], 0);
    assert_eq!(m["settled"], true, "{m}");
    assert!(m["min_planted"].as_u64().expect("count") >= 3, "{m}");
    assert!(f(&m["max_reach_ratio"]) < 1.0, "{m}");

    let control = call(
        &mut app,
        "anim.gait_simulate",
        json!({ "steps": 60, "body_path": { "points": [[0.0, 0.0], [0.0, -100.0]], "speed": 45.0 } }),
    );
    let c = &control["metrics"];
    assert!(c["replants"].as_u64().expect("replants") > 0, "{c}");
    assert!(f(&c["max_planted_slip"]) > 0.5, "jumps must show: {c}");
}

#[test]
fn gait_simulate_refusals_carry_their_codes() {
    let mut app = app();
    let cases = [
        (json!({ "steps": 20_001 }), "simulate_too_long"),
        (json!({ "steps": 5_000, "trace_every": 1 }), "simulate_too_long"),
        (json!({ "steps": 1e9 }), "simulate_too_long"),
        (json!({ "ground": { "kind": "lava" } }), "bad_ground"),
        (
            json!({ "ground": { "kind": "rubble", "cell": 0.0 } }),
            "bad_ground",
        ),
        (
            json!({ "ground": { "kind": "flat", "slope": 1.0 } }),
            "bad_ground",
        ),
        (json!({ "params": { "step_length": -1.0 } }), "gait_settings"),
        (json!({ "params": { "legs": 6 } }), "gait_settings"),
        (json!({ "dt": 0.5 }), "gait_input"),
        (json!({ "inputs": [[1.0, 0.0]] }), "gait_input"),
        (
            json!({ "body_path": { "points": [[0.0, 0.0]], "speed": 1.0 } }),
            "gait_input",
        ),
        (json!({ "params": { "patterns": "gallop" } }), "gait_pattern"),
        (
            json!({ "params": { "hip_offsets": [[-0.3, 0.5], [0.3, 0.5], [-0.3, -0.5], [0.3, -0.5]] } }),
            "gait_pattern",
        ),
        (
            json!({ "dt": 0.1, "params": { "step_length": 0.1, "forward_speed": 5.0, "acceleration": 1000.0 } }),
            "gait_step_too_large",
        ),
    ];
    for (params, code) in cases {
        let shown = params.to_string();
        assert_eq!(refused(&mut app, "anim.gait_simulate", params), code, "{shown}");
    }
}

// --- roll-up seams (ADR-8737): posture, IK LOD and `.gait.ron` in the locomotion methods ----

/// A tilting walker on a 10 degree ramp reports the stepper's own pitch, roll and quaternion,
/// and a reduced-LOD status reads `reduced`. Fails on the pre-roll-up head, where `attitude` was
/// `null` and the status had no `lod`.
#[test]
fn locomotion_state_reports_posture_and_ik_lod() {
    let mut params = hexapod();
    params.posture = GaitPosture {
        swing_weight: 0.5,
        height_tau: 0.08,
        height_slew: 0.6,
        attitude_weight: 1.0,
        max_pitch: 0.44,
        max_roll: 0.35,
        attitude_tau: 0.1,
    };
    let ramp = |_: f32, z: f32| -0.176 * z;
    let mut state = GaitState::at_rest(&params, [0.0, 0.7, 0.0], 0.0, ramp).expect("gait");
    for _ in 0..240 {
        step_gait(&params, &mut state, [1.0, 0.0, 0.0], 1.0 / 60.0, ramp).expect("step");
    }
    assert!(
        state.pitch.abs() > 0.05,
        "the ramp tilts the body: {}",
        state.pitch
    );
    let mut app = app();
    let status = FootPlacementStatus {
        probe_installed: true,
        legs_resolved: true,
        lod: IkLod::Reduced,
    };
    let rig = app.world_mut().spawn((state, status)).id();
    let out = call(
        &mut app,
        "anim.locomotion_state",
        json!({ "entity": rig.to_bits() }),
    );
    let attitude = &out["gait"]["attitude"];
    assert_eq!(attitude["pitch"], json!(state.pitch), "{attitude}");
    assert_eq!(attitude["roll"], json!(state.roll), "{attitude}");
    assert_eq!(attitude["quat"], json!(state.attitude), "{attitude}");
    assert_eq!(
        attitude["body_height_target"],
        json!(state.body_height_target),
        "{attitude}"
    );
    assert_eq!(out["foot_placement"]["lod"], "reduced");
}

const GAIT_FILE: &str = include_str!("fixtures/format-corpus/gait/v1.ron");

fn write_atomic(path: &std::path::Path, text: &str) {
    let temp = path.with_extension("tmp");
    std::fs::write(&temp, text).expect("write");
    std::fs::rename(&temp, path).expect("rename");
}

/// `gait_asset` simulates a `.gait.ron` through the engine's loader. The corpus file holds this
/// test's `hexapod()` settings, so its run equals the same walker passed as `params` (the
/// six-leg default plus the three fields that differ) bit for bit, final pose hash included.
/// Fails on the pre-roll-up head, where `gait_asset` was refused.
#[test]
fn gait_simulate_reads_a_gait_asset() {
    let project = regia_test_utils::unique_temp_dir("locomotion-gait-asset");
    std::fs::create_dir_all(project.join("assets/units")).expect("dir");
    write_atomic(&project.join("assets/units/walker.gait.ron"), GAIT_FILE);
    let tilting = GAIT_FILE.replacen(
        "    body: Some(\"thorax\"),",
        "    posture: Some((swing_weight: 0.5, height_tau: 0.08, height_slew: Some(0.6), \
         attitude_weight: 1.0, max_pitch: 0.44, max_roll: 0.35, attitude_tau: 0.1)),\n    body: Some(\"thorax\"),",
        1,
    );
    assert_ne!(tilting, GAIT_FILE, "the fixture names its body");
    write_atomic(&project.join("assets/units/tilting.gait.ron"), &tilting);
    let broken = GAIT_FILE.replacen("step_length: 0.6", "step_length: -0.6", 1);
    write_atomic(&project.join("assets/units/broken.gait.ron"), &broken);
    let newer = GAIT_FILE.replacen("format_version: 1", "format_version: 2", 1);
    write_atomic(&project.join("assets/units/newer.gait.ron"), &newer);
    write_atomic(
        &project.join("assets/units/garbage.gait.ron"),
        "(format_version: 1, nope)",
    );
    let mut app = app();
    app.insert_resource(AgentSettings {
        project_dir: Some(project.clone()),
    });
    let ground = json!({ "kind": "rubble", "amplitude": 0.08, "cell": 0.4, "seed": 11 });
    let inputs = json!([[1.0, 0.0, 0.2, 300], [0.0, 0.0, 0.0]]);

    let from_file = call(
        &mut app,
        "anim.gait_simulate",
        json!({ "steps": 480, "ground": ground, "inputs": inputs, "gait_asset": "assets/units/walker.gait.ron" }),
    );
    let from_params = call(
        &mut app,
        "anim.gait_simulate",
        json!({ "steps": 480, "ground": ground, "inputs": inputs,
                "params": { "ground_band": 50.0, "foothold_candidates": 0, "max_reach": 5.0 } }),
    );
    assert_eq!(
        from_file, from_params,
        "the file and the literal are the same walker"
    );
    let m = &from_file["metrics"];
    assert_eq!(m["refused"], 0, "{m}");
    assert!(
        m["min_planted"].as_u64().expect("count") >= 3,
        "a tripod keeps a stance: {m}"
    );

    // The posture block reaches the stepper: on a ramp the final body is pitched.
    let tilted = call(
        &mut app,
        "anim.gait_simulate",
        json!({ "steps": 240, "ground": { "kind": "slope", "rise_z": -0.176 },
                "gait_asset": "assets/units/tilting.gait.ron" }),
    );
    assert!(
        f(&tilted["final"]["attitude"]["pitch"]).abs() > 0.05,
        "{}",
        tilted["final"]
    );
    let upright = call(
        &mut app,
        "anim.gait_simulate",
        json!({ "steps": 240, "ground": { "kind": "slope", "rise_z": -0.176 },
                "gait_asset": "assets/units/walker.gait.ron" }),
    );
    assert_eq!(upright["final"]["attitude"]["pitch"], json!(0.0));

    for (params, code) in [
        (
            json!({ "gait_asset": "assets/units/broken.gait.ron" }),
            "gait_settings",
        ),
        (
            json!({ "gait_asset": "assets/units/newer.gait.ron" }),
            "gait_version",
        ),
        (
            json!({ "gait_asset": "assets/units/garbage.gait.ron" }),
            "gait_parse",
        ),
        (
            json!({ "gait_asset": "assets/units/walker.gait.ron", "params": { "step_length": 0.5 } }),
            "gait_settings",
        ),
        (json!({ "gait_asset": "assets/units/none.gait.ron" }), "F0003"),
    ] {
        let shown = params.to_string();
        assert_eq!(refused(&mut app, "anim.gait_simulate", params), code, "{shown}");
    }
    let _ = std::fs::remove_dir_all(&project);
}

// --- anim.foot_plant_simulate ----------------------------------------------------------------

/// The P-10 acceptance walk (`regia_animation/tests/foot_plant.rs`): the in-place walk skates
/// ~10 cm per stance at 1.15 times its speed; the plant lock keeps the planted ankle within
/// 1 cm.
#[test]
fn foot_plant_simulate_matches_the_acceptance_walk() {
    let mut app = app();
    let locked = call(&mut app, "anim.foot_plant_simulate", json!({}));
    let m = &locked["metrics"];
    println!("foot_plant_simulate default: {m}");
    assert!(m["stances"].as_u64().expect("stances") >= 4, "{m}");
    assert!(f(&m["max_slide"]) <= 0.01, "{m}");
    assert!(
        f(&m["max_penetration"]) < 0.01 && f(&m["max_float"]) < 0.01,
        "{m}"
    );
    assert!(f(&m["max_tilt_deg"]) < 0.5, "flat ground tilts nothing: {m}");
    let skating = call(
        &mut app,
        "anim.foot_plant_simulate",
        json!({ "settings": { "lock": false } }),
    );
    assert!(f(&skating["metrics"]["max_slide"]) > 0.05, "{skating}");
    assert_eq!(
        call(&mut app, "anim.foot_plant_simulate", json!({})),
        locked,
        "same inputs, same answer"
    );
    // A 10 degree slope tilts the planted feet.
    let slope = call(
        &mut app,
        "anim.foot_plant_simulate",
        json!({ "ground": { "kind": "slope", "rise_z": -0.176 }, "steps": 240 }),
    );
    let tilt = f(&slope["metrics"]["max_tilt_deg"]);
    assert!((5.0..=12.0).contains(&tilt), "{slope}");
}

#[test]
fn foot_plant_simulate_refusals_carry_their_codes() {
    let mut app = app();
    for (params, code) in [
        (json!({ "steps": 20_001 }), "simulate_too_long"),
        (
            json!({ "ground": { "kind": "steps", "run": -1.0 } }),
            "bad_ground",
        ),
        (json!({ "ground": "flat" }), "bad_ground"),
        (json!({ "settings": { "grip": 1.0 } }), "F0002"),
    ] {
        let shown = params.to_string();
        assert_eq!(
            refused(&mut app, "anim.foot_plant_simulate", params),
            code,
            "{shown}"
        );
    }
}

// --- anim.ik_report --------------------------------------------------------------------------

/// `hips` at 1 m; an arm `shoulder -> elbow -> hand`; a `head`; a six-joint tail.
fn ik_rig() -> Skeleton {
    let mut names = Vec::new();
    let mut parents = Vec::new();
    let mut binds = Vec::new();
    let mut push = |name: &str, parent: Option<u32>, t: Vec3| {
        names.push(name.to_string());
        parents.push(parent);
        binds.push(LocalBind {
            translation: t,
            ..LocalBind::default()
        });
    };
    push("hips", None, Vec3::new(0.0, 1.0, 0.0));
    push("shoulder", Some(0), Vec3::new(0.2, 0.3, 0.0));
    push("elbow", Some(1), Vec3::new(0.3, 0.0, 0.0));
    push("hand", Some(2), Vec3::new(0.3, 0.0, 0.0));
    push("head", Some(0), Vec3::new(0.0, 0.5, 0.0));
    for i in 0..6u32 {
        let parent = if i == 0 { 0 } else { 4 + i };
        let length = if i == 0 { 0.1 } else { 0.2 };
        push(&format!("t{i}"), Some(parent), Vec3::new(0.0, -length, 0.0));
    }
    Skeleton::from_local_bind(names, parents, binds).expect("ik rig")
}

fn ik_graph() -> AnimGraph {
    let clip = AnimNode::Clip(ClipNode {
        clip: "idle".into(),
        looping: true,
        speed: 1.0,
        sync_group: None,
    });
    let chain = AnimNode::ChainIK(ChainIKNode {
        inner: Box::new(clip),
        root: "t0".into(),
        tip: "t5".into(),
        solver: ChainSolver::Fabrik,
        target: IkTarget::Param("Tail".into()),
        pole: None,
        limits: None,
        iterations: 128,
        tolerance: 1e-5,
        weight_param: None,
    });
    let look = AnimNode::LookAt(LookAtNode {
        inner: Box::new(chain),
        joint: "head".into(),
        target: IkTarget::Param("Look".into()),
        max_yaw: 1.2,
        max_pitch: 0.8,
        weight_param: None,
    });
    AnimGraph {
        parameters: vec![
            ParamDecl("Hand".into(), ParamValue::Vec3(Vec3::new(0.55, 1.15, 0.1))),
            ParamDecl("Look".into(), ParamValue::Vec3(Vec3::new(0.3, 1.6, -2.0))),
            ParamDecl("Tail".into(), ParamValue::Vec3(Vec3::new(0.3, 0.35, 0.1))),
        ],
        root: AnimNode::TwoBoneIK(TwoBoneIKNode {
            inner: Box::new(look),
            chain: ["shoulder".into(), "elbow".into(), "hand".into()],
            target: IkTarget::Param("Hand".into()),
            pole: None,
            weight_param: None,
        }),
    }
}

fn spawn_ik_rig(app: &mut App) -> Entity {
    let world = app.world_mut();
    let binding = spawn_skeleton(world, Arc::new(ik_rig()));
    let mut clips = ClipLibrary::new();
    clips.insert(AnimationClip::empty("idle", 1.0));
    // `Animator::graph` starts from an empty parameter set: set every target.
    let mut animator = Animator::graph(ik_graph(), clips);
    animator.set("Hand", Vec3::new(0.55, 1.15, 0.1));
    animator.set("Look", Vec3::new(0.3, 1.6, -2.0));
    animator.set("Tail", Vec3::new(0.3, 0.35, 0.1));
    world.spawn((Transform::default(), binding, animator)).id()
}

#[test]
fn ik_report_off_costs_nothing() {
    let mut app = app();
    let rig = spawn_ik_rig(&mut app);
    for _ in 0..30 {
        app.update();
    }
    let out = call(&mut app, "anim.ik_report", json!({ "entity": rig.to_bits() }));
    assert_eq!(out["debug"], false);
    assert_eq!(
        out["captures"], 0,
        "30 frames with the flag off measured nothing: {out}"
    );
    assert_eq!(out["nodes"], json!([]));
    assert_eq!(out["graph"], true);
}

#[test]
fn ik_report_on_reports_residual_under_1mm() {
    let mut app = app();
    let rig = spawn_ik_rig(&mut app);
    app.update();
    let on = call(
        &mut app,
        "anim.ik_report",
        json!({ "entity": rig.to_bits(), "enable": true }),
    );
    assert_eq!(on["debug"], true);
    for _ in 0..10 {
        app.update();
    }
    let out = call(&mut app, "anim.ik_report", json!({ "entity": rig.to_bits() }));
    assert_eq!(out["captures"], 30, "three nodes, ten frames: {out}");
    let nodes = out["nodes"].as_array().expect("nodes");
    let kinds: Vec<(&str, &str)> = nodes
        .iter()
        .map(|n| {
            (
                n["path"].as_str().expect("path"),
                n["kind"].as_str().expect("kind"),
            )
        })
        .collect();
    assert_eq!(
        kinds,
        [
            ("root", "two_bone_ik"),
            ("root/inner", "look_at"),
            ("root/inner/inner", "chain_ik")
        ]
    );
    for node in nodes {
        assert_eq!(node["solved"], true, "{node}");
        assert_eq!(node["clamped"], false, "{node}");
        assert!(f(&node["residual"]) < 1.0e-3, "{node}");
        assert_eq!(node["weight"], json!(1.0));
        assert_eq!(node["graph_weight"], json!(1.0));
    }
    // Off again: the counter stops.
    call(
        &mut app,
        "anim.ik_report",
        json!({ "entity": rig.to_bits(), "enable": false }),
    );
    app.update();
    let off = call(&mut app, "anim.ik_report", json!({ "entity": rig.to_bits() }));
    assert_eq!(off["captures"], 30);
    assert_eq!(off["nodes"], json!([]));
}

#[test]
fn ik_report_refuses_missing_entities_and_rigs_without_animator() {
    let mut app = app();
    let gone = app.world_mut().spawn_empty().id();
    assert!(app.world_mut().despawn(gone));
    assert_eq!(
        refused(&mut app, "anim.ik_report", json!({ "entity": gone.to_bits() })),
        "no_such_entity"
    );
    let plain = app.world_mut().spawn(Transform::default()).id();
    assert_eq!(
        refused(&mut app, "anim.ik_report", json!({ "entity": plain.to_bits() })),
        "no_animator"
    );
}
