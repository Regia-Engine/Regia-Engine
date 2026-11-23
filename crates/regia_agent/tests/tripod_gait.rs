// SPDX-License-Identifier: MPL-2.0
//! The tripod gait is observable and controllable through existing agent methods.
use regia_agent::{LocalReply, call_local};
use regia_animation::AnimationPlugin;
use regia_animation::gait::{
    GaitParams, GaitPatternSet, GaitPosture, GaitState, MAX_LEGS, leg_array, step_gait,
};
use regia_app::prelude::{App, MinimalPlugins};
use serde_json::{Value, json};

fn call(app: &mut App, method: &str, params: Value) -> Value {
    match call_local(app.world_mut(), method, params, Some("tripod-test")) {
        LocalReply::Done(Ok(value)) => value,
        reply => panic!("{method}: {reply:?}"),
    }
}

#[test]
fn inspect_assert_and_freeze_tripod_through_dispatcher() {
    let params = GaitParams {
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
        // Ground settings (ADR-8792): wide band and reach, ring search off (the
        // pre-search behavior these scenarios were written for).
        ground_band: 50.0,
        foothold_candidates: 0,
        foothold_spacing: 0.25,
        max_reach: [5.0; MAX_LEGS],
        patterns: GaitPatternSet::tripod(),
        posture: GaitPosture::UPRIGHT,
        hang_drop: 0.3,
    };
    let mut state = GaitState::at_rest(&params, [0.0; 3], 0.0, |_, _| 0.0).expect("valid gait fixture");
    step_gait(&params, &mut state, [1.0, 0.0, 0.0], 0.1, |_, _| 0.0).expect("valid gait fixture");
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, AnimationPlugin));
    let entity = app.world_mut().spawn(state).id();
    let id = entity.to_bits();
    let query = call(
        &mut app,
        "scene.query_entities",
        json!({"component": "GaitState"}),
    );
    assert_eq!(query["entities"].as_array().expect("valid gait fixture").len(), 1);
    let value = call(
        &mut app,
        "scene.get_entity",
        json!({"entity": id, "components": ["GaitState"]}),
    );
    let gait = &value["components"]["GaitState"]["value"];
    assert_eq!(gait["swing_a"], true);
    // Per-foot arrays are MAX_LEGS long (ADR-8653); `legs` says how many are used.
    assert_eq!(
        gait["feet"].as_array().expect("valid gait fixture").len(),
        MAX_LEGS
    );
    assert_eq!(gait["legs"], 6);
    assert!(
        gait["lift_off"].is_array(),
        "inspection must include swing history: {value}"
    );
    let assertions: Vec<_> = [1, 2, 5].into_iter().map(|i| json!({
        "name": format!("foot {i} planted"),
        "condition": {"entity": id, "component": "GaitState", "path": format!("feet[{i}].planted"), "op": "eq", "value": true}
    })).collect();
    let result = call(&mut app, "play.expect", json!({"assertions": assertions}));
    assert_eq!(result["passed"], 3, "{result}");
    assert_eq!(result["failed"], 0);
    call(
        &mut app,
        "scene.set_property",
        json!({"entity": id, "component": "GaitState", "path": "power_lost", "value": true}),
    );
    let frozen = *app.world().get::<GaitState>(entity).expect("valid gait fixture");
    let mut state = app
        .world_mut()
        .get_mut::<GaitState>(entity)
        .expect("valid gait fixture");
    step_gait(&params, &mut state, [1.0, 0.5, 0.2], 0.1, |_, _| {
        panic!("frozen gait must not sample")
    })
    .expect("valid gait fixture");
    assert_eq!(*state, frozen);
    let guide = call(&mut app, "agent.guide", json!({"topic": "tripod-gait"}));
    assert!(guide.to_string().contains("gait_step_too_large"));
}

#[test]
fn driven_mode_and_replants_are_asserted_through_dispatcher() {
    use regia_animation::gait::{GaitBody, step_gait_driven};
    let params = GaitParams {
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
        // Ground settings (ADR-8792): wide band and reach, ring search off (the
        // pre-search behavior these scenarios were written for).
        ground_band: 50.0,
        foothold_candidates: 0,
        foothold_spacing: 0.25,
        max_reach: [5.0; MAX_LEGS],
        patterns: GaitPatternSet::tripod(),
        posture: GaitPosture::UPRIGHT,
        hang_drop: 0.3,
    };
    let mut body = GaitBody {
        position: [0.0; 3],
        yaw: 0.0,
        velocity: [0.0, 0.0, -1.5],
    };
    let mut state = GaitState::at_rest_driven(&params, body, |_, _| 0.0).expect("valid gait fixture");
    for _ in 0..90 {
        body.position[2] += body.velocity[2] / 60.0;
        step_gait_driven(&params, &mut state, body, 1.0 / 60.0, |_, _| 0.0).expect("valid gait fixture");
    }
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, AnimationPlugin));
    let entity = app.world_mut().spawn(state).id();
    let id = entity.to_bits();
    let walk = json!({"assertions": [
        {"name": "driven by the sim", "condition": {"entity": id, "component": "GaitState", "path": "mode", "op": "eq", "value": "Driven"}},
        {"name": "no pops over the walk", "condition": {"entity": id, "component": "GaitState", "path": "replants", "op": "eq", "value": 0}}
    ]});
    let result = call(&mut app, "play.expect", walk.clone());
    assert_eq!(
        (result["passed"].as_u64(), result["failed"].as_u64()),
        (Some(2), Some(0)),
        "{result}"
    );
    // A sim teleport replants once and the same assertion now reports it.
    body.position[0] += 5.0 * params.step_length;
    {
        let mut state = app
            .world_mut()
            .get_mut::<GaitState>(entity)
            .expect("valid gait fixture");
        step_gait_driven(&params, &mut state, body, 1.0 / 60.0, |_, _| 0.0).expect("valid gait fixture");
    }
    let result = call(&mut app, "play.expect", walk);
    assert_eq!(
        (result["passed"].as_u64(), result["failed"].as_u64()),
        (Some(1), Some(1)),
        "{result}"
    );
    let value = call(
        &mut app,
        "scene.get_entity",
        json!({"entity": id, "components": ["GaitState"]}),
    );
    assert_eq!(
        value["components"]["GaitState"]["value"]["replants"], 1,
        "{value}"
    );
    let guide = call(&mut app, "agent.guide", json!({"topic": "tripod-gait"}));
    assert!(guide.to_string().contains("step_gait_driven"));
}

#[test]
fn settled_is_asserted_through_dispatcher() {
    use regia_animation::gait::{GaitBody, step_gait_driven};
    let params = GaitParams {
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
        // Ground settings (ADR-8792): wide band and reach, ring search off (the
        // pre-search behavior these scenarios were written for).
        ground_band: 50.0,
        foothold_candidates: 0,
        foothold_spacing: 0.25,
        max_reach: [5.0; MAX_LEGS],
        patterns: GaitPatternSet::tripod(),
        posture: GaitPosture::UPRIGHT,
        hang_drop: 0.3,
    };
    let dt = 1.0 / 60.0;
    let mut body = GaitBody {
        position: [0.0; 3],
        yaw: 0.0,
        velocity: [0.0, 0.0, -1.5],
    };
    let mut state = GaitState::at_rest_driven(&params, body, |_, _| 0.0).expect("valid gait fixture");
    for _ in 0..80 {
        body.position[2] += body.velocity[2] * dt;
        step_gait_driven(&params, &mut state, body, dt, |_, _| 0.0).expect("valid gait fixture");
    }
    body.velocity = [0.0; 3];
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, AnimationPlugin));
    let entity = app.world_mut().spawn(state).id();
    let id = entity.to_bits();
    let settled = json!({"assertions": [
        {"name": "settled after the stop", "condition": {"entity": id, "component": "GaitState", "path": "settled", "op": "eq", "value": true}}
    ]});
    let result = call(&mut app, "play.expect", settled.clone());
    assert_eq!(result["failed"], 1, "a walking gait is not settled: {result}");
    // The book's promise: after stopping, settled is true within two settle periods.
    let frames = (2.0 * params.settle_period / dt).ceil() as usize + 1;
    let step = |app: &mut App| {
        let mut state = app
            .world_mut()
            .get_mut::<GaitState>(entity)
            .expect("valid gait fixture");
        step_gait_driven(&params, &mut state, body, dt, |_, _| 0.0).expect("valid gait fixture");
    };
    for _ in 0..frames {
        step(&mut app);
    }
    let result = call(&mut app, "play.expect", settled.clone());
    assert_eq!(result["passed"], 1, "{result}");
    let value = call(
        &mut app,
        "scene.get_entity",
        json!({"entity": id, "components": ["GaitState"]}),
    );
    let feet = value["components"]["GaitState"]["value"]["feet"].clone();
    assert_eq!(feet.as_array().map(Vec::len), Some(MAX_LEGS), "{value}");
    // No foot moves after: the reflected feet are identical 120 steps later.
    for _ in 0..120 {
        step(&mut app);
    }
    let value = call(
        &mut app,
        "scene.get_entity",
        json!({"entity": id, "components": ["GaitState"]}),
    );
    assert_eq!(value["components"]["GaitState"]["value"]["feet"], feet);
    assert_eq!(call(&mut app, "play.expect", settled)["passed"], 1);
    let guide = call(&mut app, "agent.guide", json!({"topic": "tripod-gait"}));
    assert!(guide.to_string().contains("settled"));
}

#[test]
fn ground_counters_are_asserted_through_dispatcher() {
    use regia_animation::gait::{GaitBody, GaitGround, GroundHit, step_gait_driven};
    /// The book's adapter shape: level ground only for x <= -0.2, nothing past the edge.
    #[derive(Clone, Copy)]
    struct Edge;
    impl GaitGround for Edge {
        fn sample(&self, x: f32, _: f32, _: f32, _: f32) -> Option<GroundHit> {
            (x <= -0.2).then_some(GroundHit {
                y: 0.0,
                normal: [0.0, 1.0, 0.0],
            })
        }
    }
    let params = GaitParams {
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
        ground_band: 1.0,
        foothold_candidates: 16,
        foothold_spacing: 0.25,
        max_reach: [1.1; MAX_LEGS],
        patterns: GaitPatternSet::tripod(),
        posture: GaitPosture::UPRIGHT,
        hang_drop: 0.3,
    };
    let dt = 1.0 / 60.0;
    let walk = |ground: &dyn Fn(&mut GaitState, GaitBody), state: &mut GaitState| {
        let mut body = GaitBody {
            position: [0.0, params.clearance, 0.0],
            yaw: 0.0,
            velocity: [0.0, 0.0, -1.5],
        };
        for _ in 0..120 {
            body.position[2] += body.velocity[2] * dt;
            ground(state, body);
        }
    };
    let body = GaitBody {
        position: [0.0, params.clearance, 0.0],
        yaw: 0.0,
        velocity: [0.0; 3],
    };
    // A flat walk: no forced steps, no refusals, level normals.
    let mut flat = GaitState::at_rest_driven(&params, body, |_, _| 0.0).expect("valid gait fixture");
    walk(
        &|s, b| step_gait_driven(&params, s, b, dt, |_, _| 0.0).expect("valid gait fixture"),
        &mut flat,
    );
    // Along a cliff edge: the right feet hang and every refusal counts.
    let mut edge = GaitState::at_rest_driven(&params, body, Edge).expect("no ground is not an error");
    walk(
        &|s, b| step_gait_driven(&params, s, b, dt, Edge).expect("no ground is not an error"),
        &mut edge,
    );
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, AnimationPlugin));
    let flat_id = app.world_mut().spawn(flat).id().to_bits();
    let edge_id = app.world_mut().spawn(edge).id().to_bits();
    let on_flat = json!({"assertions": [
        {"name": "no forced steps on the flat", "condition": {"entity": flat_id, "component": "GaitState", "path": "forced_steps", "op": "eq", "value": 0}},
        {"name": "no refused footholds", "condition": {"entity": flat_id, "component": "GaitState", "path": "refused", "op": "eq", "value": 0}},
        {"name": "foot 0 on level ground", "condition": {"entity": flat_id, "component": "GaitState", "path": "feet[0].normal[1]", "op": "eq", "value": 1.0}}
    ]});
    let result = call(&mut app, "play.expect", on_flat);
    assert_eq!(
        (result["passed"].as_u64(), result["failed"].as_u64()),
        (Some(3), Some(0)),
        "{result}"
    );
    let at_edge = json!({"assertions": [
        {"name": "right front foot hangs at the edge", "condition": {"entity": edge_id, "component": "GaitState", "path": "foot_modes[1]", "op": "eq", "value": "Hanging"}},
        {"name": "hanging is not planted", "condition": {"entity": edge_id, "component": "GaitState", "path": "feet[1].planted", "op": "eq", "value": false}},
        {"name": "refusals counted", "condition": {"entity": edge_id, "component": "GaitState", "path": "refused", "op": "gte", "value": 3}}
    ]});
    let result = call(&mut app, "play.expect", at_edge);
    assert_eq!(
        (result["passed"].as_u64(), result["failed"].as_u64()),
        (Some(3), Some(0)),
        "{result}"
    );
    let value = call(
        &mut app,
        "scene.get_entity",
        json!({"entity": edge_id, "components": ["GaitState"]}),
    );
    let gait = &value["components"]["GaitState"]["value"];
    assert_eq!(gait["forced_steps"], 0, "{value}");
    assert_eq!(
        gait["foot_modes"].as_array().map(Vec::len),
        Some(MAX_LEGS),
        "{value}"
    );
    let guide = call(&mut app, "agent.guide", json!({"topic": "tripod-gait"}));
    assert!(guide.to_string().contains("forced_steps"));
}

/// Gait patterns (ADR-8653): the pattern in effect, per-foot phase and the stability margin
/// are reflected, assertable through `play.expect`, and the guide names `gait_pattern`.
#[test]
fn pattern_fields_are_asserted_through_dispatcher() {
    use regia_animation::gait::{GaitBody, GaitPattern, step_gait_driven};
    let params = GaitParams {
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
        hip_offsets: leg_array([[-0.3, 0.5], [0.3, 0.5], [-0.3, -0.5], [0.3, -0.5]], [0.0; 2]),
        settle_distance: 0.05,
        settle_period: 0.3,
        turn_step_distance: 0.15,
        turn_step_angle: 0.35,
        ground_band: 50.0,
        foothold_candidates: 0,
        foothold_spacing: 0.25,
        max_reach: [f32::INFINITY; MAX_LEGS],
        hang_drop: 0.3,
        patterns: GaitPatternSet::single(GaitPattern::trot()),
        posture: GaitPosture::UPRIGHT,
    };
    let mut body = GaitBody {
        position: [0.0; 3],
        yaw: 0.0,
        velocity: [0.0, 0.0, -1.5],
    };
    let mut state = GaitState::at_rest_driven(&params, body, |_, _| 0.0).expect("valid gait fixture");
    for _ in 0..40 {
        body.position[2] -= 1.5 / 60.0;
        step_gait_driven(&params, &mut state, body, 1.0 / 60.0, |_, _| 0.0).expect("valid gait fixture");
    }
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, AnimationPlugin));
    let id = app.world_mut().spawn(state).id().to_bits();
    let value = call(
        &mut app,
        "scene.get_entity",
        json!({"entity": id, "components": ["GaitState"]}),
    );
    let gait = &value["components"]["GaitState"]["value"];
    assert_eq!(gait["legs"], 4, "{value}");
    assert_eq!(gait["pattern"]["duty_factor"], 0.5, "{value}");
    assert_eq!(
        gait["foot_phase"].as_array().map(Vec::len),
        Some(MAX_LEGS),
        "{value}"
    );
    assert!(gait["stability_margin"].is_number(), "{value}");
    let condition = |name: &str, path: &str, op: &str, value: Value| json!({"name": name, "condition": {"entity": id, "component": "GaitState", "path": path, "op": op, "value": value}});
    let result = call(
        &mut app,
        "play.expect",
        json!({"assertions": [
            condition("a trot is statically unstable", "stability_margin", "lte", json!(0.0)),
            condition("one pattern", "pattern_index", "eq", json!(0)),
            condition("not switching", "blending", "eq", json!(false)),
            condition("foot phase in range", "foot_phase[0]", "lt", json!(1.0)),
        ]}),
    );
    assert_eq!(result["passed"], 4, "{result}");
    for topic in ["procedural-gait", "tripod-gait"] {
        let guide = call(&mut app, "agent.guide", json!({"topic": topic}));
        assert_eq!(guide["topic"], "procedural-gait");
        assert!(guide.to_string().contains("gait_pattern"), "{guide}");
        assert!(guide.to_string().contains("stability_margin"), "{guide}");
    }
}

/// Posture (ADR-8706): on a 10 degree ramp the body pitches with the ground; pitch, roll,
/// attitude and body_height_target are reflected and assertable through `play.expect`, and
/// the guide documents them.
#[test]
fn posture_fields_are_asserted_through_dispatcher() {
    use regia_animation::gait::{GaitBody, step_gait_driven};
    let params = GaitParams {
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
                [-0.4, -0.5],
                [0.4, -0.5],
                [-0.5, 0.0],
                [0.5, 0.0],
                [-0.4, 0.5],
                [0.4, 0.5],
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
        hang_drop: 0.3,
        patterns: GaitPatternSet::tripod(),
        posture: GaitPosture {
            swing_weight: 0.5,
            height_tau: 0.08,
            height_slew: 0.6,
            attitude_weight: 1.0,
            max_pitch: 0.44,
            max_roll: 0.35,
            attitude_tau: 0.1,
        },
    };
    // Driven mode walks -Z; the ground rises 10 degrees that way.
    let slope = 10.0_f32.to_radians().tan();
    let ramp = move |_: f32, z: f32| -slope * z;
    let mut body = GaitBody {
        position: [0.0, 0.7, 0.0],
        yaw: 0.0,
        velocity: [0.0, 0.0, -1.0],
    };
    let mut state = GaitState::at_rest_driven(&params, body, ramp).expect("valid gait fixture");
    for _ in 0..120 {
        body.position[2] -= 1.0 / 60.0;
        step_gait_driven(&params, &mut state, body, 1.0 / 60.0, ramp).expect("valid gait fixture");
    }
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, AnimationPlugin));
    let id = app.world_mut().spawn(state).id().to_bits();
    let value = call(
        &mut app,
        "scene.get_entity",
        json!({"entity": id, "components": ["GaitState"]}),
    );
    let gait = &value["components"]["GaitState"]["value"];
    assert_eq!(gait["attitude"].as_array().map(Vec::len), Some(4), "{value}");
    assert!(gait["body_height_target"].is_number(), "{value}");
    let condition = |name: &str, path: &str, op: &str, value: Value| json!({"name": name, "condition": {"entity": id, "component": "GaitState", "path": path, "op": op, "value": value}});
    let result = call(
        &mut app,
        "play.expect",
        json!({"assertions": [
            condition("nose up on the ramp", "pitch", "gt", json!(0.17)),
            condition("not past 10.5 degrees", "pitch", "lt", json!(0.184)),
            condition("no roll", "roll", "lt", json!(0.01)),
            condition("tilted, not identity", "attitude[3]", "lt", json!(1.0)),
            condition("height target above the floor", "body_height_target", "gt", json!(0.7)),
        ]}),
    );
    assert_eq!(result["passed"], 5, "{result}");
    let guide = call(&mut app, "agent.guide", json!({"topic": "procedural-gait"}));
    for word in ["attitude", "body_height_target", "attitude_weight", "ADR-8706"] {
        assert!(guide.to_string().contains(word), "{word}: {guide}");
    }
}

/// ADR-8736: in a bound's flight phase (no foot planted) `stability_margin` is the finite
/// `NO_SUPPORT_MARGIN`, so the book's `lte 0.0` assertion holds through the dispatcher
/// instead of failing on JSON null; and an agent write of an invalid `pattern.duty_factor`
/// makes the host's next step refuse with `gait_pattern`, leaving the state unchanged.
#[test]
fn flight_margin_and_pattern_writes_through_dispatcher() {
    use regia_animation::gait::{GaitBody, GaitError, GaitPattern, NO_SUPPORT_MARGIN, step_gait_driven};
    let params = GaitParams {
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
        hip_offsets: leg_array([[-0.3, 0.5], [0.3, 0.5], [-0.3, -0.5], [0.3, -0.5]], [0.0; 2]),
        settle_distance: 0.05,
        settle_period: 0.3,
        turn_step_distance: 0.15,
        turn_step_angle: 0.35,
        ground_band: 50.0,
        foothold_candidates: 0,
        foothold_spacing: 0.25,
        max_reach: [f32::INFINITY; MAX_LEGS],
        hang_drop: 0.3,
        patterns: GaitPatternSet::single(GaitPattern::bound()),
        posture: GaitPosture::UPRIGHT,
    };
    let mut body = GaitBody {
        position: [0.0; 3],
        yaw: 0.0,
        velocity: [0.0, 0.0, -2.0],
    };
    let mut state = GaitState::at_rest_driven(&params, body, |_, _| 0.0).expect("valid gait fixture");
    let mut steps = 0;
    while state.feet().iter().any(|f| f.planted) {
        body.position[2] -= 2.0 / 60.0;
        step_gait_driven(&params, &mut state, body, 1.0 / 60.0, |_, _| 0.0).expect("valid gait fixture");
        steps += 1;
        assert!(steps < 300, "a bound at 2 m/s has a flight phase");
    }
    assert_eq!(state.stability_margin, NO_SUPPORT_MARGIN);
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, AnimationPlugin));
    let entity = app.world_mut().spawn(state).id();
    let id = entity.to_bits();
    let value = call(
        &mut app,
        "scene.get_entity",
        json!({"entity": id, "components": ["GaitState"]}),
    );
    let gait = &value["components"]["GaitState"]["value"];
    assert!(gait["stability_margin"].is_number(), "{value}");
    let result = call(
        &mut app,
        "play.expect",
        json!({"assertions": [
            {"name": "statically unstable in flight", "condition": {"entity": id, "component": "GaitState", "path": "stability_margin", "op": "lte", "value": 0.0}},
            {"name": "nothing planted", "condition": {"entity": id, "component": "GaitState", "path": "feet[0].planted", "op": "eq", "value": false}}
        ]}),
    );
    assert_eq!(
        (result["passed"].as_u64(), result["failed"].as_u64()),
        (Some(2), Some(0)),
        "{result}"
    );
    call(
        &mut app,
        "scene.set_property",
        json!({"entity": id, "component": "GaitState", "path": "pattern.duty_factor", "value": 0.0}),
    );
    let mut written = *app.world().get::<GaitState>(entity).expect("valid gait fixture");
    assert_eq!(written.pattern.duty_factor, 0.0);
    let before = written;
    body.position[2] -= 2.0 / 60.0;
    let error = step_gait_driven(&params, &mut written, body, 1.0 / 60.0, |_, _| 0.0);
    assert_eq!(error, Err(GaitError::Pattern));
    assert_eq!(written, before);
    assert!(GaitError::Pattern.to_string().starts_with("gait_pattern:"));
}
