// SPDX-License-Identifier: MPL-2.0
//! Skin-palette and joint-global reuse, and the LOD skip of a frame that did not evaluate.
//!
//! A gated frame must not rewrite joint transforms or replace the palette. An evaluating frame
//! must fill the palette the rig already owns, so a spare allocation stays put.

use std::sync::Arc;
use std::time::Duration;

use regia_animation::components::spawn_skeleton;
use regia_animation::prelude::*;
use regia_animation::skeleton::LocalBind;
use regia_app::prelude::*;
use regia_core::math::{Mat4, Vec3};
use regia_scene::Transform;

fn chain() -> Skeleton {
    Skeleton::from_local_bind(
        vec!["root".into(), "child".into()],
        vec![None, Some(0)],
        vec![
            LocalBind {
                translation: Vec3::ZERO,
                ..LocalBind::default()
            },
            LocalBind {
                translation: Vec3::Y,
                ..LocalBind::default()
            },
        ],
    )
    .expect("skeleton")
}

fn app_with_rig(mode: UpdateMode) -> (App, Entity) {
    let mut app = App::new();
    app.add_plugins((regia_app::MinimalPlugins, AnimationPlugin));
    app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
        Duration::from_millis(100),
    ));
    // The first frame has no delta. Warm up before the rig exists.
    app.update();
    let animator = Animator::single(AnimationClip::empty("idle", 10.0), true);
    let mut animator = animator;
    animator.update_mode = mode;
    let binding = spawn_skeleton(app.world_mut(), Arc::new(chain()));
    let rig = app
        .world_mut()
        .spawn((Transform::default(), binding, animator))
        .id();
    (app, rig)
}

fn palette_ptr(app: &App, rig: Entity) -> (*const Mat4, usize) {
    let matrices = &app.world().get::<SkinPalette>(rig).expect("palette").matrices;
    (matrices.as_ptr(), matrices.capacity())
}

fn joint_translation(app: &App, rig: Entity, joint: usize) -> Vec3 {
    let mesh = app.world().get::<SkinnedMesh>(rig).expect("mesh");
    app.world()
        .get::<Transform>(mesh.joints[joint])
        .expect("joint")
        .translation
}

/// One real evaluation, then a far LOD rate of 0. The joint a test wrote must stay, and the
/// palette allocation must stay.
#[test]
fn lod_skip_does_not_rewrite_joints_or_replace_the_palette() {
    let (mut app, rig) = app_with_rig(UpdateMode::Always);
    app.update();
    // End each `Mut` borrow before the next world query. `Mut` does not implement `Drop`.
    let (ptr, cap) = {
        let mut palette = app
            .world_mut()
            .get_mut::<SkinPalette>(rig)
            .expect("palette after the first evaluation");
        palette.matrices.reserve(64);
        (palette.matrices.as_ptr(), palette.matrices.capacity())
    };
    let (gptr, gcap) = {
        let mut globals = app
            .world_mut()
            .get_mut::<JointGlobals>(rig)
            .expect("globals after the first evaluation");
        globals.globals.reserve(64);
        (globals.globals.as_ptr(), globals.globals.capacity())
    };

    let child = app.world().get::<SkinnedMesh>(rig).expect("mesh").joints[1];
    {
        let mut joint = app.world_mut().get_mut::<Transform>(child).expect("child joint");
        joint.translation.x = 42.0;
    }
    {
        let mut animator = app.world_mut().get_mut::<Animator>(rig).expect("animator");
        animator.update_mode = UpdateMode::Lod {
            near: 0.0,
            mid: 0.0,
            far_rate: 0.0,
        };
    }
    app.insert_resource(AnimLodCamera {
        position: Vec3::new(100.0, 0.0, 0.0),
    });
    app.update();

    assert_eq!(
        joint_translation(&app, rig, 1).x,
        42.0,
        "a frame that did not evaluate must not rewrite joint transforms"
    );
    let (ptr2, cap2) = palette_ptr(&app, rig);
    assert_eq!(ptr2, ptr, "a gated frame must not replace the skin palette");
    assert_eq!(cap2, cap, "a gated frame must keep the palette's spare capacity");
    let globals = &app.world().get::<JointGlobals>(rig).expect("globals").globals;
    assert_eq!(globals.as_ptr(), gptr);
    assert_eq!(globals.capacity(), gcap);
}

/// Two evaluating frames fill the same palette allocation.
#[test]
fn evaluating_frames_reuse_the_palette_allocation() {
    let (mut app, rig) = app_with_rig(UpdateMode::Always);
    app.update();
    {
        let mut palette = app.world_mut().get_mut::<SkinPalette>(rig).expect("palette");
        palette.matrices.reserve(64);
    }
    let (ptr, cap) = palette_ptr(&app, rig);
    app.update();
    let (ptr2, cap2) = palette_ptr(&app, rig);
    assert_eq!(ptr2, ptr, "the next evaluation must fill the existing palette");
    assert_eq!(
        cap2, cap,
        "spare palette capacity must survive the next evaluation"
    );
    assert!(cap2 > 2, "the test reserved spare capacity past the joint count");
}
