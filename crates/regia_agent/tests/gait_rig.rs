// SPDX-License-Identifier: MPL-2.0
//! `GaitRig` (ADR-8707) is observable and controllable through existing agent methods.
use std::sync::Arc;
use std::time::Duration;

use regia_agent::{LocalReply, call_local};
use regia_animation::clip::AnimationClip;
use regia_animation::components::spawn_skeleton;
use regia_animation::gait::{
    GaitParams, GaitPatternSet, GaitPosture, GaitState, MAX_LEGS, TRIPOD_LEGS, leg_array,
};
use regia_animation::gait_rig::{GaitControls, GaitLeg, GaitRig, GaitRigMode};
use regia_animation::skeleton::LocalBind;
use regia_animation::{AnimationPlugin, Animator, Skeleton};
use regia_app::prelude::{App, MinimalPlugins, TimeUpdateStrategy};
use regia_core::math::Vec3;
use regia_scene::Transform;
use serde_json::{Value, json};

fn call(app: &mut App, method: &str, params: Value) -> Value {
    match call_local(app.world_mut(), method, params, Some("gait-rig-test")) {
        LocalReply::Done(Ok(value)) => value,
        reply => panic!("{method}: {reply:?}"),
    }
}

/// A hexapod with two-bone legs: tips on the floor, body 0.5 m up, −Z forward.
fn hexapod() -> Arc<Skeleton> {
    let mut names = vec!["root".to_string(), "body".to_string()];
    let mut parents = vec![None, Some(0)];
    let mut binds = vec![
        LocalBind::default(),
        LocalBind {
            translation: Vec3::new(0.0, 0.5, 0.0),
            ..LocalBind::default()
        },
    ];
    for leg in 0..TRIPOD_LEGS {
        let side = if leg % 2 == 0 { -1.0 } else { 1.0 };
        let z = [-0.5, 0.0, 0.5][leg / 2];
        let base = names.len() as u32;
        for (j, (name, t)) in [
            (format!("hip{leg}"), Vec3::new(side * 0.25, 0.0, z)),
            (format!("knee{leg}"), Vec3::new(side * 0.2, 0.15, 0.0)),
            (format!("foot{leg}"), Vec3::new(side * 0.15, -0.65, 0.0)),
        ]
        .into_iter()
        .enumerate()
        {
            names.push(name);
            parents.push(Some(if j == 0 { 1 } else { base + j as u32 - 1 }));
            binds.push(LocalBind {
                translation: t,
                ..LocalBind::default()
            });
        }
    }
    Arc::new(Skeleton::from_local_bind(names, parents, binds).expect("hexapod"))
}

fn params() -> GaitParams {
    GaitParams {
        forward_speed: 0.6,
        reverse_speed: 0.3,
        strafe_speed: 0.3,
        acceleration: 2.0,
        step_length: 0.3,
        clearance: 0.5,
        swing_lift: 0.1,
        max_slope: 1.5,
        idle_speed: 0.02,
        max_yaw_rate: 0.8,
        hip_offsets: leg_array([[0.0; 2]; TRIPOD_LEGS], [0.0; 2]),
        settle_distance: 0.05,
        settle_period: 0.3,
        turn_step_distance: 0.1,
        turn_step_angle: 0.35,
        ground_band: 0.6,
        foothold_candidates: 0,
        foothold_spacing: 0.2,
        max_reach: [f32::INFINITY; MAX_LEGS],
        hang_drop: 0.2,
        patterns: GaitPatternSet::tripod(),
        posture: GaitPosture::UPRIGHT,
    }
}

#[test]
fn gait_rig_is_inspected_and_driven_through_dispatcher() {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, AnimationPlugin));
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f32(
        1.0 / 60.0,
    )));
    let skeleton = hexapod();
    let binding = spawn_skeleton(app.world_mut(), skeleton);
    let legs = (0..TRIPOD_LEGS)
        .map(|i| GaitLeg::new(format!("hip{i}"), format!("foot{i}")))
        .collect();
    let rig = GaitRig::new(params(), legs)
        .with_body("body")
        .with_mode(GaitRigMode::Integrating);
    let entity = app
        .world_mut()
        .spawn((
            Transform::default(),
            binding,
            Animator::single(AnimationClip::empty("still", 1.0), true),
            rig,
            GaitControls::default(),
        ))
        .id();
    let id = entity.to_bits();
    for _ in 0..3 {
        app.update();
    }
    let value = call(
        &mut app,
        "scene.get_entity",
        json!({"entity": id, "components": ["GaitRig", "GaitState", "GaitRigStatus"]}),
    );
    let components = &value["components"];
    assert_eq!(
        components["GaitRig"]["value"]["legs"][2]["tip"], "foot2",
        "{value}"
    );
    assert_eq!(components["GaitState"]["value"]["legs"], 6, "{value}");
    assert_eq!(
        components["GaitRigStatus"]["value"]["legs_resolved"], 6,
        "{value}"
    );
    let healthy = call(
        &mut app,
        "play.expect",
        json!({"assertions": [
            {"name": "all legs resolved", "condition": {"entity": id, "component": "GaitRigStatus", "path": "legs_resolved", "op": "eq", "value": 6}},
            {"name": "no step refused", "condition": {"entity": id, "component": "GaitRigStatus", "path": "step_errors", "op": "eq", "value": 0}},
            {"name": "standing", "condition": {"entity": id, "component": "GaitState", "path": "settled", "op": "eq", "value": true}}
        ]}),
    );
    assert_eq!(healthy["failed"], 0, "{healthy}");
    // Drive it: forward through the controls component, then the rig walks on its own.
    call(
        &mut app,
        "scene.set_property",
        json!({"entity": id, "component": "GaitControls", "path": "forward", "value": 1.0}),
    );
    for _ in 0..120 {
        app.update();
    }
    let z = app
        .world()
        .get::<Transform>(entity)
        .expect("transform")
        .translation
        .z;
    assert!(z < -0.5, "the rig walked forward (-Z): z = {z}");
    let walking = call(
        &mut app,
        "play.expect",
        json!({"assertions": [
            {"name": "not settled while walking", "condition": {"entity": id, "component": "GaitState", "path": "settled", "op": "eq", "value": false}},
            {"name": "no replants", "condition": {"entity": id, "component": "GaitState", "path": "replants", "op": "eq", "value": 0}}
        ]}),
    );
    assert_eq!(walking["failed"], 0, "{walking}");
    assert!(app.world().get::<GaitState>(entity).is_some());
    let guide = call(&mut app, "agent.guide", json!({"topic": "procedural-gait"}));
    assert!(guide.to_string().contains("gait_rig_leg_unresolved"));
}
