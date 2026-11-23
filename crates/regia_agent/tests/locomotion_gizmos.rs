// SPDX-License-Identifier: MPL-2.0
//! The `locomotion` gizmo overlay (ADR-8729) through the agent dispatcher: `debug.gizmos` lists
//! the group with its sub-toggles, `debug.set_gizmos` turns it on and flips sub-toggles, and the
//! overlay draws what the toggles say. Fails to compile on the base, which has no overlay.

use std::sync::Arc;
use std::time::Duration;

use regia_agent::{LocalReply, call_local};
use regia_animation::clip::AnimationClip;
use regia_animation::components::spawn_skeleton;
use regia_animation::gait::{GaitParams, GaitPatternSet, GaitPosture, MAX_LEGS, TRIPOD_LEGS, leg_array};
use regia_animation::gait_rig::{GaitControls, GaitLeg, GaitRig, GaitRigMode};
use regia_animation::skeleton::LocalBind;
use regia_animation::{AnimationPlugin, Animator, Skeleton};
use regia_app::prelude::{App, MinimalPlugins, Render, TimeUpdateStrategy};
use regia_core::math::Vec3;
use regia_framework::locomotion_gizmos::{LocomotionGizmoStats, LocomotionGizmosPlugin};
use regia_render::gizmos::GizmoPlugin;
use regia_scene::Transform;
use serde_json::{Value, json};

fn call(app: &mut App, method: &str, params: Value) -> Value {
    match call_local(app.world_mut(), method, params, Some("locomotion-gizmos-test")) {
        LocalReply::Done(Ok(value)) => value,
        reply => panic!("{method}: {reply:?}"),
    }
}

fn call_err(app: &mut App, method: &str, params: Value) -> String {
    match call_local(app.world_mut(), method, params, Some("locomotion-gizmos-test")) {
        LocalReply::Done(Err(error)) => format!("{error:?}"),
        reply => panic!("{method}: expected an error, got {reply:?}"),
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

fn locomotion(listed: &Value) -> Value {
    listed["groups"]
        .as_array()
        .and_then(|groups| groups.iter().find(|g| g["name"] == "locomotion"))
        .cloned()
        .unwrap_or_else(|| panic!("locomotion listed: {listed}"))
}

fn stats(app: &App) -> LocomotionGizmoStats {
    *app.world().resource::<LocomotionGizmoStats>()
}

#[test]
fn debug_set_gizmos_toggles_locomotion() {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, AnimationPlugin));
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f32(
        1.0 / 60.0,
    )));
    app.init_resource::<regia_render::ExtractedFrame>()
        .init_resource::<regia_render::RuntimeRenderer>()
        .add_systems(Render, regia_render::runtime::extract_frame)
        .add_systems(Render, regia_render::runtime::render_texture_views)
        .add_plugins(GizmoPlugin)
        .add_plugins(LocomotionGizmosPlugin);
    let binding = spawn_skeleton(app.world_mut(), hexapod());
    let legs = (0..TRIPOD_LEGS)
        .map(|i| GaitLeg::new(format!("hip{i}"), format!("foot{i}")))
        .collect();
    app.world_mut().spawn((
        Transform::default(),
        binding,
        Animator::single(AnimationClip::empty("still", 1.0), true),
        GaitRig::new(params(), legs)
            .with_body("body")
            .with_mode(GaitRigMode::Integrating),
        GaitControls {
            forward: 1.0,
            ..GaitControls::default()
        },
    ));
    for _ in 0..5 {
        app.update();
    }

    // Listed, off, with its three sub-toggles on.
    let listed = locomotion(&call(&mut app, "debug.gizmos", json!({})));
    assert_eq!(listed["enabled"], json!(false), "{listed}");
    assert_eq!(
        listed["toggles"],
        json!({ "gait": true, "foot_placement": true, "ik": true }),
        "{listed}"
    );
    assert_eq!(stats(&app).frames, 0, "off: nothing ran");

    // On, without IK.
    let set = call(
        &mut app,
        "debug.set_gizmos",
        json!({ "group": "locomotion", "enabled": true, "toggles": { "ik": false } }),
    );
    assert_eq!(set["before"]["enabled"], json!(false), "{set}");
    assert_eq!(set["before"]["toggles"]["ik"], json!(true), "{set}");
    assert_eq!(set["after"]["enabled"], json!(true), "{set}");
    assert_eq!(
        set["after"]["toggles"],
        json!({ "gait": true, "foot_placement": true, "ik": false }),
        "{set}"
    );
    app.update();
    let s = stats(&app);
    assert_eq!((s.frames, s.walkers, s.chains), (1, 1, 0), "{s:?}");
    assert!(s.lines > 0 && s.strips > 0, "{s:?}");

    // Toggles alone: IK back on, gait off.
    call(
        &mut app,
        "debug.set_gizmos",
        json!({ "group": "locomotion", "toggles": { "ik": true, "gait": false } }),
    );
    app.update();
    let s = stats(&app);
    assert_eq!((s.walkers, s.chains), (0, 6), "{s:?}");
    let listed = locomotion(&call(&mut app, "debug.gizmos", json!({})));
    assert_eq!(listed["toggles"]["gait"], json!(false), "{listed}");

    // Refused, and nothing changes.
    for (params, needle) in [
        (
            json!({ "group": "locomotion", "toggles": { "feet": true } }),
            "unknown `locomotion` sub-toggle",
        ),
        (
            json!({ "group": "locomotion", "toggles": { "ik": "yes" } }),
            "must be a boolean",
        ),
        (
            json!({ "group": "locomotion", "toggles": ["ik"] }),
            "must be an object",
        ),
        (
            json!({ "group": "physics", "toggles": { "ik": true } }),
            "has no sub-toggles",
        ),
    ] {
        let error = call_err(&mut app, "debug.set_gizmos", params.clone());
        assert!(error.contains(needle), "{params}: {error}");
    }
    let listed = locomotion(&call(&mut app, "debug.gizmos", json!({})));
    assert_eq!(
        listed["toggles"],
        json!({ "gait": false, "foot_placement": true, "ik": true }),
        "{listed}"
    );

    // Off again: the overlay stops running.
    call(
        &mut app,
        "debug.set_gizmos",
        json!({ "group": "locomotion", "enabled": false }),
    );
    let frames = stats(&app).frames;
    for _ in 0..3 {
        app.update();
    }
    assert_eq!(stats(&app).frames, frames);
}
