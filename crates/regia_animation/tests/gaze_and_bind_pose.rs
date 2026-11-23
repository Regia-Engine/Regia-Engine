// SPDX-License-Identifier: MPL-2.0
//! End-to-end tests for eye gaze (C6) and per-instance bind poses (C9) through the plugin.

use std::sync::Arc;

use regia_animation::components::spawn_skeleton;
use regia_animation::gaze::{EyeChannel, EyeSide, GazeTarget, LidMorphMap, LidSpec};
use regia_animation::pose::translated;
use regia_animation::prelude::*;
use regia_animation::{Gaze, GazeRig, InstanceBindPose, Skeleton};
use regia_app::prelude::*;
use regia_core::math::{Mat4, Quat, Vec3};
use regia_scene::Transform;

/// root → spine → head → eye_l / eye_r / upper_lid_l.
fn skeleton() -> Skeleton {
    let names = ["root", "spine", "head", "eye_l", "eye_r", "upper_lid_l"];
    let parents = vec![None, Some(0), Some(1), Some(2), Some(2), Some(2)];
    let binds = vec![
        translated(Vec3::new(0.0, 1.0, 0.0)),
        translated(Vec3::new(0.0, 0.3, 0.0)),
        translated(Vec3::new(0.0, 0.3, 0.0)),
        translated(Vec3::new(-0.03, 0.05, -0.08)),
        translated(Vec3::new(0.03, 0.05, -0.08)),
        translated(Vec3::new(-0.03, 0.07, -0.08)),
    ];
    Skeleton::from_local_bind(names.iter().map(|s| s.to_string()).collect(), parents, binds)
        .expect("valid skeleton")
}

fn app() -> App {
    let mut app = App::new();
    app.add_plugins((regia_app::MinimalPlugins, AnimationPlugin));
    app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
        std::time::Duration::from_millis(16),
    ));
    app
}

fn spawn_rig(app: &mut App, skeleton: &Arc<Skeleton>, at: Vec3) -> (Entity, SkinnedMesh) {
    let rig = app.world_mut().spawn(Transform::from_translation(at)).id();
    let binding = spawn_skeleton(app.world_mut(), skeleton.clone());
    app.world_mut().entity_mut(rig).insert(binding.clone());
    let mut clip = AnimationClip::empty("still", 1.0);
    clip.tracks.clear();
    app.world_mut()
        .entity_mut(rig)
        .insert(Animator::single(clip, true));
    (rig, binding)
}

#[test]
fn gaze_turns_the_eyes_and_fills_morph_weights_through_the_plugin() {
    let mut app = app();
    let skeleton = Arc::new(skeleton());
    let (rig, binding) = spawn_rig(&mut app, &skeleton, Vec3::ZERO);
    let mut gaze = Gaze::new({
        let mut r = GazeRig::from_skeleton(&skeleton).expect("eyes by name");
        r.lids = vec![LidSpec::new(5, EyeSide::Left, 0.7)];
        r
    });
    gaze.morphs = LidMorphMap::from_arkit_names(&["eyeLookUpLeft".to_string(), "eyeLookOutLeft".to_string()]);
    let target = Vec3::new(-1.0, 2.4, -3.0);
    gaze.target = GazeTarget::Point(target);
    app.world_mut().entity_mut(rig).insert(gaze);

    for _ in 0..12 {
        app.update();
    }
    // The eye joint's local rotation points the eye at the target.
    let eye = app
        .world()
        .get::<Transform>(binding.joints[3])
        .copied()
        .expect("eye");
    assert!(eye.rotation.angle_between(Quat::IDENTITY) > 0.1, "eye turned");
    let globals = app
        .world()
        .get::<JointGlobals>(rig)
        .expect("globals")
        .globals
        .clone();
    let forward = globals[3].to_scale_rotation_translation().1 * Vec3::NEG_Z;
    let want = (target - globals[3].w_axis.truncate()).normalize();
    assert!(forward.angle_between(want) < 0.02, "eye aims at the target");

    // The lid followed and the morph channels arrived in the published weights.
    let lid = app
        .world()
        .get::<Transform>(binding.joints[5])
        .copied()
        .expect("lid");
    assert!(lid.rotation.angle_between(Quat::IDENTITY) > 0.05, "lid followed");
    let weights = app.world().get::<MorphWeights>(rig).expect("weights");
    assert!(weights.0[0] > 0.1, "up channel {:?}", weights.0);
    assert!(weights.0[1] > 0.1, "out channel {:?}", weights.0);
    let result = *app.world().get::<Gaze>(rig).expect("gaze").result();
    assert!(result.valid && result.error < 0.02);
    assert!(result.channels[0].get(EyeChannel::Up) > 0.1);

    // Remove the target: the eyes go back to rest.
    app.world_mut().get_mut::<Gaze>(rig).expect("gaze").target = GazeTarget::None;
    for _ in 0..30 {
        app.update();
    }
    let eye = app
        .world()
        .get::<Transform>(binding.joints[3])
        .copied()
        .expect("eye");
    assert!(eye.rotation.angle_between(Quat::IDENTITY) < 1e-3);
}

#[test]
fn gaze_follows_an_entity_and_the_rig_transform() {
    let mut app = app();
    let skeleton = Arc::new(skeleton());
    // The rig stands at x = 10 turned 90° to the left; the target is straight ahead of it.
    let (rig, binding) = spawn_rig(&mut app, &skeleton, Vec3::new(10.0, 0.0, 0.0));
    app.world_mut().get_mut::<Transform>(rig).expect("rig").rotation =
        Quat::from_rotation_y(std::f32::consts::FRAC_PI_2);
    let ahead = app
        .world_mut()
        .spawn(Transform::from_translation(Vec3::new(5.0, 1.65, 0.0)))
        .id();
    let mut gaze = Gaze::new(GazeRig::from_skeleton(&skeleton).expect("eyes"));
    gaze.target = GazeTarget::Entity(ahead);
    app.world_mut().entity_mut(rig).insert(gaze);
    for _ in 0..12 {
        app.update();
    }
    // A 90° left turn faces −X: the target at x = 5 is dead ahead, so the eyes stay at rest.
    let eye = app
        .world()
        .get::<Transform>(binding.joints[3])
        .copied()
        .expect("eye");
    assert!(
        eye.rotation.angle_between(Quat::IDENTITY) < 0.02,
        "{:?}",
        eye.rotation
    );
}

#[test]
fn instance_bind_poses_share_a_clip_with_different_proportions() {
    let mut app = app();
    let skeleton = Arc::new(skeleton());
    let (plain, plain_binding) = spawn_rig(&mut app, &skeleton, Vec3::ZERO);
    let (tall, tall_binding) = spawn_rig(&mut app, &skeleton, Vec3::new(2.0, 0.0, 0.0));
    let (copy, _) = spawn_rig(&mut app, &skeleton, Vec3::new(4.0, 0.0, 0.0));
    app.world_mut()
        .entity_mut(tall)
        .insert(InstanceBindPose::uniform(&skeleton, 1.25).expect("instance"));
    app.world_mut()
        .entity_mut(copy)
        .insert(InstanceBindPose::uniform(&skeleton, 1.0).expect("instance"));
    for _ in 0..4 {
        app.update();
    }
    let head = |app: &App, binding: &SkinnedMesh| {
        app.world()
            .get::<Transform>(binding.joints[2])
            .copied()
            .expect("head")
            .translation
    };
    // Local head offsets scale with the instance.
    assert!((head(&app, &tall_binding) - head(&app, &plain_binding) * 1.25).length() < 1e-4);

    // At the rest pose every palette is identity regardless of proportions.
    for rig in [plain, tall, copy] {
        let palette = app.world().get::<SkinPalette>(rig).expect("palette");
        assert!(
            palette
                .matrices
                .iter()
                .all(|m| m.abs_diff_eq(Mat4::IDENTITY, 1e-4)),
            "rig {rig:?}"
        );
    }
    // The joint globals (what sockets read) follow the instance.
    let tall_globals = &app.world().get::<JointGlobals>(tall).expect("globals").globals;
    let plain_globals = &app.world().get::<JointGlobals>(plain).expect("globals").globals;
    let tall_top = tall_globals[2].w_axis.truncate();
    let plain_top = plain_globals[2].w_axis.truncate();
    assert!((tall_top - plain_top * 1.25).length() < 1e-4);

    // The shared skeleton asset is untouched.
    assert_eq!(skeleton.bind_pose[2].translation, Vec3::new(0.0, 0.3, 0.0));
}

#[test]
fn a_mismatched_instance_is_ignored_not_fatal() {
    let mut app = app();
    let skeleton = Arc::new(skeleton());
    let (rig, _) = spawn_rig(&mut app, &skeleton, Vec3::ZERO);
    let other =
        Skeleton::from_local_bind(vec!["a".into()], vec![None], vec![Default::default()]).expect("skeleton");
    app.world_mut()
        .entity_mut(rig)
        .insert(InstanceBindPose::uniform(&other, 2.0).expect("instance"));
    for _ in 0..3 {
        app.update();
    }
    let palette = app.world().get::<SkinPalette>(rig).expect("palette");
    assert_eq!(palette.matrices.len(), 6);
    assert!(
        palette
            .matrices
            .iter()
            .all(|m| m.abs_diff_eq(Mat4::IDENTITY, 1e-4))
    );
}

/// Review w11 #4: head-follow gaze runs before the rig drivers, so a driver reading the head's
/// rotation sees the gazed head (it used to read the clip's head, at rest here).
#[test]
fn rig_drivers_see_the_gaze_turned_head() {
    use regia_animation::aim_ik::AimChain;
    use regia_animation::curve::{CurveKey, CurveKeyKind};
    use regia_animation::driver::{DriverInput, DriverOutput};
    use regia_animation::gaze::HeadFollow;
    use regia_animation::{Driver, DriverSet, NamedMorphs, RigDrivers};

    let mut app = app();
    let skeleton = Arc::new(skeleton());
    let (rig, _) = spawn_rig(&mut app, &skeleton, Vec3::ZERO);
    // A driver turning the head's total rotation (degrees) into a morph weight, 1:1.
    let set = DriverSet {
        drivers: vec![Driver {
            name: "head_turn".into(),
            input: DriverInput::JointRotation { joint: "head".into() },
            curve: vec![
                CurveKey::new(0.0, 0.0, CurveKeyKind::Linear),
                CurveKey::new(180.0, 180.0, CurveKeyKind::Linear),
            ]
            .into(),
            output: DriverOutput::Morph("head_turned".into()),
            blend: Default::default(),
            weight: 1.0,
        }],
        ..DriverSet::default()
    };
    let compiled = set
        .compile(&skeleton, &NamedMorphs::from_set(&set))
        .expect("drivers compile");
    let mut rig_desc = GazeRig::from_skeleton(&skeleton).expect("eyes");
    rig_desc.head = Some(HeadFollow {
        chain: AimChain {
            joints: vec![2],
            shares: Vec::new(),
            limits: Vec::new(),
            aim_joint: 2,
            aim_axis: Vec3::NEG_Z,
        },
        weight: 1.0,
    });
    let mut gaze = Gaze::new(rig_desc);
    // Far to the left at eye height: the head turns about 45 degrees.
    gaze.target = GazeTarget::Point(Vec3::new(-3.0, 1.65, -3.0));
    app.world_mut()
        .entity_mut(rig)
        .insert((gaze, RigDrivers::new(Arc::new(compiled))));
    for _ in 0..4 {
        app.update();
    }
    let weights = app.world().get::<MorphWeights>(rig).expect("weights");
    assert!(
        weights.0.first().is_some_and(|w| *w > 30.0),
        "the driver read the gazed head (about 45 degrees): {:?}",
        weights.0
    );
}

/// Review w11 #5: a rig with its own proportions (25 % taller) aims its eyes at a near target
/// from where its eyes are drawn, not from the shared skeleton's eye height.
#[test]
fn a_taller_instance_gazes_from_its_own_eyes() {
    let mut app = app();
    let skeleton = Arc::new(skeleton());
    let (tall, _) = spawn_rig(&mut app, &skeleton, Vec3::ZERO);
    app.world_mut()
        .entity_mut(tall)
        .insert(InstanceBindPose::uniform(&skeleton, 1.25).expect("instance"));
    // The instance's eyes sit at about 2.06 m (shared: 1.65 m); the target is 0.6 m ahead,
    // 0.2 m to the left and level with them, so the shared eye height would pitch them up ~34°.
    let target = Vec3::new(-0.2, 2.0625, -0.7);
    let mut gaze = Gaze::new(GazeRig::from_skeleton(&skeleton).expect("eyes"));
    gaze.target = GazeTarget::Point(target);
    app.world_mut().entity_mut(tall).insert(gaze);
    for _ in 0..12 {
        app.update();
    }
    let globals = app
        .world()
        .get::<JointGlobals>(tall)
        .expect("globals")
        .globals
        .clone();
    for eye in [3usize, 4] {
        let at = globals[eye].w_axis.truncate();
        assert!((at.y - 2.0625).abs() < 1e-3, "instance eye height {at}");
        let forward = globals[eye].to_scale_rotation_translation().1 * Vec3::NEG_Z;
        let want = (target - at).normalize();
        let err = forward.angle_between(want);
        assert!(err < 0.02, "eye {eye} misses the target by {err} rad");
    }
}
