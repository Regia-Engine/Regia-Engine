// SPDX-License-Identifier: MPL-2.0
//! Character rig systems end to end (Tincal C4, C5, C7): twist distribution along a forearm,
//! drivers and pose-space correctives writing the morph bank, helper joints, and skeleton
//! profile conformance, through the animation plugin.

use std::sync::Arc;

use regia_animation::clip::{JointTrack, TrackCurve};
use regia_animation::components::spawn_skeleton;
use regia_animation::corrective::{CompiledCorrective, CorrectivePose, PoseSpaceCorrective};
use regia_animation::skeleton::LocalBind;
use regia_animation::twist::swing_twist;
use regia_animation::{
    AnimationClip, AnimationPlugin, Animator, DriverSet, MorphWeights, NamedMorphs, RigDrivers, Skeleton,
    SkeletonProfile, SkinnedMesh,
};
use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3, det};
use regia_scene::Transform;

const FIXTURE: &str = include_str!("fixtures/arm.drivers.ron");

fn arm_skeleton() -> Skeleton {
    let at = |y: f32| LocalBind {
        translation: Vec3::new(0.0, y, 0.0),
        ..LocalBind::default()
    };
    Skeleton::from_local_bind(
        ["root", "upper", "fore", "twist1", "twist2", "hand", "pad"]
            .map(String::from)
            .to_vec(),
        vec![None, Some(0), Some(1), Some(2), Some(2), Some(2), Some(0)],
        vec![at(0.0), at(0.3), at(0.3), at(0.1), at(0.2), at(0.3), at(0.1)],
    )
    .expect("valid skeleton")
}

/// A clip holding the hand rolled 90 degrees about its bone axis and the forearm bent 100 degrees.
fn pose_clip() -> AnimationClip {
    let mut clip = AnimationClip::empty("reach", 1.0);
    let hold = |joint: u32, rotation: Quat| JointTrack {
        joint,
        translations: TrackCurve::Static(Vec3::new(
            0.0,
            [0.0, 0.3, 0.3, 0.1, 0.2, 0.3, 0.1][joint as usize],
            0.0,
        )),
        rotations: TrackCurve::Static(rotation),
        scales: TrackCurve::Static(Vec3::ONE),
    };
    clip.tracks
        .push(hold(2, Quat::from_rotation_x(100f32.to_radians())));
    clip.tracks
        .push(hold(5, Quat::from_rotation_y(90f32.to_radians())));
    clip
}

fn spawn_rig(app: &mut App, drivers: Option<&DriverSet>) -> Entity {
    let skeleton = Arc::new(arm_skeleton());
    let binding = spawn_skeleton(app.world_mut(), skeleton.clone());
    let rig = app
        .world_mut()
        .spawn((Transform::default(), binding, Animator::single(pose_clip(), true)))
        .id();
    if let Some(set) = drivers {
        let morphs = NamedMorphs::from_set(set);
        let compiled = set.compile(&skeleton, &morphs).expect("fixture compiles");
        app.world_mut()
            .entity_mut(rig)
            .insert(RigDrivers::new(Arc::new(compiled)));
    }
    rig
}

fn joint_rotation(app: &App, rig: Entity, name: &str) -> Quat {
    let binding = app.world().get::<SkinnedMesh>(rig).expect("binding");
    let index = binding.skeleton.joint_index(name).expect("joint") as usize;
    app.world()
        .get::<Transform>(binding.joints[index])
        .expect("joint transform")
        .rotation
}

fn app() -> App {
    let mut app = App::new();
    app.add_plugins((regia_app::MinimalPlugins, AnimationPlugin));
    app
}

fn run(app: &mut App) {
    for _ in 0..4 {
        app.update();
    }
}

fn roll(q: Quat) -> f32 {
    swing_twist(q, Vec3::Y).1.to_degrees()
}

#[test]
fn twist_joints_split_the_wrist_roll_so_no_step_exceeds_a_third() {
    let set = DriverSet::from_ron_str(FIXTURE).expect("fixture parses");
    let mut app = app();
    let with = spawn_rig(&mut app, Some(&set));
    let without = spawn_rig(&mut app, None);
    run(&mut app);

    let chain =
        |rig: Entity| ["fore", "twist1", "twist2", "hand"].map(|j| roll(joint_rotation(&app, rig, j)));
    let step = |rolls: [f32; 4]| {
        // Roll accumulated along the chain: fore sets the start; each joint adds its own.
        // The twist joints are siblings of the hand under the forearm, so the rings sit at
        // 0 (forearm), t1, t2, hand: the visible pinch is the largest jump between rings.
        let mut previous = 0.0f32;
        let mut worst = 0.0f32;
        for r in [0.0, rolls[1], rolls[2], rolls[3]] {
            worst = worst.max((r - previous).abs());
            previous = r;
        }
        worst
    };
    let driven = chain(with);
    let control = chain(without);
    assert!(
        (driven[1] - 30.0).abs() < 0.5 && (driven[2] - 60.0).abs() < 0.5,
        "{driven:?}"
    );
    assert!(
        (driven[3] - 90.0).abs() < 0.5,
        "the hand keeps its roll: {driven:?}"
    );
    assert!(
        control[1].abs() < 0.5 && control[2].abs() < 0.5,
        "no rig: {control:?}"
    );
    let (a, b) = (step(driven), step(control));
    assert!(
        a <= 30.5,
        "with twist joints the largest step is a third of the roll: {a}"
    );
    assert!(b >= 89.0, "without them the whole roll lands in one step: {b}");
}

#[test]
fn drivers_correctives_and_params_write_the_morph_bank() {
    let set = DriverSet::from_ron_str(FIXTURE).expect("fixture parses");
    let mut app = app();
    let rig = spawn_rig(&mut app, Some(&set));
    app.world_mut()
        .get_mut::<Animator>(rig)
        .expect("animator")
        .parameters
        .set_float("smile", 0.5);
    run(&mut app);

    let names = set.morph_names();
    let index = |n: &str| names.iter().position(|m| m == n).expect("morph name");
    let weights = app.world().get::<MorphWeights>(rig).expect("weights").0.clone();
    assert!((weights[index("elbow_bulge")] - 1.0).abs() < 1e-3, "{weights:?}");
    assert!((weights[index("mouth_smile")] - 0.4).abs() < 1e-4, "{weights:?}");
    assert!((weights[index("elbow_crease")] - 1.0).abs() < 1e-2, "{weights:?}");

    let state = &app.world().get::<RigDrivers>(rig).expect("drivers").state;
    assert!((state.materials[0] - 1.0).abs() < 1e-3, "{:?}", state.materials);
    assert!((state.twist_degrees[0] - 90.0).abs() < 0.5);
    assert!((state.helper_degrees[0] - 100.0).abs() < 0.5);
    // The helper slid along its offset path at 100 degrees.
    let pad = {
        let binding = app.world().get::<SkinnedMesh>(rig).expect("binding");
        app.world()
            .get::<Transform>(binding.joints[6])
            .expect("pad")
            .translation
    };
    assert!((pad.z - 0.03).abs() < 1e-4 && (pad.y - 0.1).abs() < 1e-4, "{pad}");
}

#[test]
fn the_animators_own_pose_is_not_changed_by_the_rig() {
    let set = DriverSet::from_ron_str(FIXTURE).expect("fixture parses");
    let mut app = app();
    let rig = spawn_rig(&mut app, Some(&set));
    run(&mut app);
    let pose = &app
        .world()
        .get::<Animator>(rig)
        .expect("animator")
        .evaluated_pose();
    // The twist joints are still at their evaluated (identity) rotation in the animator's pose.
    assert!(roll(pose.rotations[3]).abs() < 1e-3 && roll(pose.rotations[4]).abs() < 1e-3);
}

#[test]
fn the_fixture_round_trips_through_ron() {
    let set = DriverSet::from_ron_str(FIXTURE).expect("fixture parses");
    let text = set.to_ron_string().expect("writes");
    assert_eq!(DriverSet::from_ron_str(&text).expect("reads"), set);
    assert_eq!(set.twists.len(), 1);
    assert_eq!(set.drivers.len(), 3);
    assert_eq!(set.correctives.len(), 1);
}

#[test]
fn a_project_profile_checks_a_skeleton_and_feeds_the_retargeter() {
    // A small project profile (the shape a 67-joint project profile has): custom names, roles
    // from the VRM humanoid set, one optional joint, aliases for imports.
    let profile = SkeletonProfile::from_ron_str(
        r#"(
            format_version: 1,
            name: "arm-rig",
            humanoid: false,
            allow_extra_joints: true,
            joints: [
                (name: "root", role: Some("hips")),
                (name: "upper", role: Some("leftUpperArm"), parent: Some("root"), aliases: ["upperarm_l"]),
                (name: "fore", role: Some("leftLowerArm"), parent: Some("upper")),
                (name: "hand", role: Some("leftHand"), parent: Some("fore")),
                (name: "pad", parent: Some("root"), required: false),
            ],
        )"#,
    )
    .expect("parses");
    assert_eq!(profile.validate(), Ok(()));
    let skeleton = arm_skeleton();
    let report = profile.check_skeleton(&skeleton);
    assert!(report.ok(), "{report:?}");
    assert_eq!(report.matched, 5);
    assert_eq!(report.extra_joints, ["twist1", "twist2"]);
    let retarget = profile.retarget_profile(&skeleton);
    assert_eq!(retarget.roles["leftLowerArm"], "fore");

    let mut strict = profile.clone();
    strict.allow_extra_joints = false;
    let report = strict.check_skeleton(&skeleton);
    assert!(!report.ok());
    assert_eq!(report.forbidden_extra, ["twist1", "twist2"]);
}

/// A corrective whose "plus"/"minus" examples are mirrored about the watched joint's
/// bind rotation: every kernel distance treats the pair identically (bit for bit), so
/// the Gaussian elimination meets an exact f64 tie in the eliminated matrix (audit-v040
/// M6). The "x0" example sits about a radian away so the matrix stays solvable.
fn mirrored_corrective() -> PoseSpaceCorrective {
    PoseSpaceCorrective {
        name: "mirror".into(),
        joints: vec!["fore".into()],
        poses: vec![
            CorrectivePose {
                name: "x0".into(),
                rotations: vec![Vec3::new(0.0, 57.29578, 0.0)],
                morphs: vec![("tie".into(), 0.3)],
            },
            CorrectivePose {
                name: "a".into(),
                rotations: vec![Vec3::new(0.0, 0.003, 0.0)],
                morphs: vec![],
            },
            CorrectivePose {
                name: "plus".into(),
                rotations: vec![Vec3::new(0.003, 0.0, 0.0)],
                morphs: vec![("tie".into(), 1.0)],
            },
            CorrectivePose {
                name: "minus".into(),
                rotations: vec![Vec3::new(-0.003, 0.0, 0.0)],
                morphs: vec![("tie".into(), 0.7)],
            },
        ],
        radius: 0.8,
        weight: 1.0,
    }
}

#[test]
fn corrective_pivot_breaks_ties_by_lowest_row() {
    let skeleton = arm_skeleton();
    let compiled = CompiledCorrective::compile(&mirrored_corrective(), &skeleton, &|name: &str| {
        (name == "tie").then_some(0)
    })
    .expect("compiles");
    // Pose the watched joint at the "a" example: the mirrored pair sits at bit-identical
    // kernel distances, an exact tie the deterministic pivot must resolve the same way
    // on every platform.
    let mut pose = skeleton.bind_pose_pose();
    pose.rotations[2] = det::quat_from_euler_xyz(0.0, 0.003f32.to_radians(), 0.0);
    let mut morphs = [0.0f32; 1];
    let mut activations = Vec::new();
    compiled.apply(&pose, &mut morphs, &mut activations);
    assert_eq!(activations.len(), 4);
    assert_eq!(
        activations[2].to_bits(),
        activations[3].to_bits(),
        "the mirrored pair ties exactly"
    );
    // The solved weight, pinned bit for bit. The mirrored pair's exact tie is provably
    // unobservable at f32 (the eliminated matrix is near-degenerate either way; the f64-level
    // rule is locked by `pivot_ties_keep_the_lowest_row` on the solver), so this pin records
    // the end-to-end f32 output of the det pipeline (audit-v040 M6).
    assert_eq!(morphs[0].to_bits(), 0x3ef0_0000);
}

#[test]
fn corrective_example_quaternions_are_platform_stable() {
    let skeleton = arm_skeleton();
    let corrective = PoseSpaceCorrective {
        name: "elbow".into(),
        joints: vec!["fore".into()],
        poses: vec![
            CorrectivePose {
                name: "bent90".into(),
                rotations: vec![Vec3::new(90.0, 0.0, 0.0)],
                morphs: vec![("bulge".into(), 0.5)],
            },
            CorrectivePose {
                name: "bent140".into(),
                rotations: vec![Vec3::new(140.0, 0.0, 0.0)],
                morphs: vec![("bulge".into(), 1.0)],
            },
        ],
        radius: 0.8,
        weight: 1.0,
    };
    let compiled = CompiledCorrective::compile(&corrective, &skeleton, &|name: &str| {
        (name == "bulge").then_some(0)
    })
    .expect("compiles");
    let mut pose = skeleton.bind_pose_pose();
    pose.rotations[2] = det::quat_from_euler_xyz(45f32.to_radians(), 0.0, 0.0);
    let mut morphs = [0.0f32; 1];
    let mut activations = Vec::new();
    compiled.apply(&pose, &mut morphs, &mut activations);
    // The 90° example's activation, rebuilt through the same det math the engine uses
    // (bit-stable on every platform), must match the compiled one exactly.
    let q45 = det::quat_from_euler_xyz(45f32.to_radians(), 0.0, 0.0);
    let q90 = det::quat_from_euler_xyz(90f32.to_radians(), 0.0, 0.0);
    let angle = 2.0 * det::acos(q45.dot(q90).abs().min(1.0));
    let distance = (angle * angle).sqrt();
    let t = distance / 0.8;
    let expected = det::exp(-t * t);
    assert_eq!(activations[0], expected);
    assert_eq!(activations[0].to_bits(), 0x3ec3_4abe);
}
