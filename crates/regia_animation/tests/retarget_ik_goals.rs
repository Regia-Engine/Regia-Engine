// SPDX-License-Identifier: MPL-2.0
//! IK goals at retarget bake (ADR-8476, `docs/formats/retarget-ron.md` v2): a retargeted clip
//! keeps the source's planted feet and the gap of hands on a prop on a rig of other proportions.
//!
//! The rigs are built in code: a source with 0.8 m legs and a target with 1.1 m legs (thigh and
//! shin split differently, a lower ankle, so the hip-height scale is not the leg ratio), both in
//! the demo rig's joint layout, so `demo_locomotion` authors the demo walk on the source.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::ops::Range;

use regia_animation::clip::{AnimationClip, JointTrack, TrackCurve};
use regia_animation::demo_rig::{DEMO_PARENTS, DemoGait, DemoRig, RigNaming, demo_humanoid, demo_locomotion};
use regia_animation::ik::{IkChain, solve_two_bone};
use regia_animation::motion_matching::ContactSettings;
use regia_animation::motion_matching::contacts::label_contacts;
use regia_animation::retarget::{IkGoalMode, IkGoalSpec, RetargetOptions, RetargetProfile, Retargeter};
use regia_animation::retarget_setup::{RETARGET_FORMAT_VERSION, RetargetSetup, migrate_retarget};
use regia_animation::skeleton::{LocalBind, Skeleton};
use regia_core::math::{Quat, Vec3};

const NAMES: [&str; 20] = [
    "root",
    "hips",
    "spine",
    "chest",
    "neck",
    "head",
    "left_upper_arm",
    "left_forearm",
    "left_hand",
    "right_upper_arm",
    "right_forearm",
    "right_hand",
    "left_thigh",
    "left_calf",
    "left_foot",
    "left_toes",
    "right_thigh",
    "right_calf",
    "right_foot",
    "right_toes",
];

/// Body proportions of a code-built rig (meters, T-pose, facing −Z, left on −X).
struct Body {
    ankle: f32,
    thigh: f32,
    calf: f32,
    hip_x: f32,
    toe: f32,
    shoulder: Vec3,
    upper_arm: f32,
    forearm: f32,
}

const SOURCE: Body = Body {
    ankle: 0.1,
    thigh: 0.4,
    calf: 0.4,
    hip_x: 0.1,
    toe: 0.12,
    shoulder: Vec3::new(0.18, 1.35, 0.0),
    upper_arm: 0.28,
    forearm: 0.28,
};

const TARGET: Body = Body {
    ankle: 0.06,
    thigh: 0.6,
    calf: 0.5,
    hip_x: 0.12,
    toe: 0.15,
    shoulder: Vec3::new(0.22, 1.72, 0.0),
    upper_arm: 0.3,
    forearm: 0.36,
};

fn rig(body: &Body) -> Skeleton {
    let hip_y = body.ankle + body.thigh + body.calf;
    let hips = hip_y + 0.02;
    let sh = body.shoulder;
    let left = |v: Vec3| Vec3::new(-v.x, v.y, v.z);
    let leg = |x: f32| {
        [
            Vec3::new(x, hip_y, 0.0),
            Vec3::new(x, hip_y - body.thigh, 0.0),
            Vec3::new(x, body.ankle, 0.0),
            Vec3::new(x, 0.0, -body.toe),
        ]
    };
    let arm = [
        sh,
        sh + Vec3::X * body.upper_arm,
        sh + Vec3::X * (body.upper_arm + body.forearm),
    ];
    let [lt, lc, lf, lo] = leg(-body.hip_x);
    let [rt, rc, rf, ro] = leg(body.hip_x);
    let points: [Vec3; 20] = [
        Vec3::ZERO,
        Vec3::new(0.0, hips, 0.0),
        Vec3::new(0.0, hips + 0.1, 0.0),
        Vec3::new(0.0, sh.y - 0.17, 0.0),
        Vec3::new(0.0, sh.y + 0.05, 0.0),
        Vec3::new(0.0, sh.y + 0.15, 0.0),
        left(arm[0]),
        left(arm[1]),
        left(arm[2]),
        arm[0],
        arm[1],
        arm[2],
        lt,
        lc,
        lf,
        lo,
        rt,
        rc,
        rf,
        ro,
    ];
    let binds = (0..20)
        .map(|i| {
            let parent = DEMO_PARENTS[i].map_or(Vec3::ZERO, |p| points[p as usize]);
            LocalBind {
                translation: points[i] - parent,
                rotation: Quat::IDENTITY,
                scale: Vec3::ONE,
            }
        })
        .collect();
    Skeleton::from_local_bind(
        NAMES.iter().map(|n| n.to_string()).collect(),
        DEMO_PARENTS.to_vec(),
        binds,
    )
    .expect("rig")
}

fn walk(source: &Skeleton) -> AnimationClip {
    let gait = DemoGait {
        name: "walk".into(),
        speed: 1.3,
        cycle: 1.0,
        duty: 0.55,
    };
    demo_locomotion(source, &[gait]).remove(0)
}

fn retargeter(source: &Skeleton, target: &Skeleton, goals: Vec<IkGoalSpec>) -> Retargeter {
    let options = RetargetOptions {
        ik_goals: goals,
        ..RetargetOptions::default()
    };
    Retargeter::with_options(
        source,
        &RetargetProfile::guess_humanoid(source),
        target,
        &RetargetProfile::guess_humanoid(target),
        &options,
    )
    .expect("pairs")
}

fn feet_goals(mode: IkGoalMode) -> Vec<IkGoalSpec> {
    vec![
        IkGoalSpec::new(["leftUpperLeg", "leftLowerLeg", "leftFoot"], mode),
        IkGoalSpec::new(["rightUpperLeg", "rightLowerLeg", "rightFoot"], mode),
    ]
}

/// The baked clip's key times (`Retargeter::retarget_clip` samples `frames` evenly).
fn key_times(clip: &AnimationClip) -> Vec<f32> {
    let frames = (clip.duration * clip.sample_rate).ceil() as usize + 1;
    (0..frames)
        .map(|k| k as f32 / (frames - 1) as f32 * clip.duration)
        .collect()
}

/// Object-space positions of `joints` at `times`.
fn track(skeleton: &Skeleton, clip: &AnimationClip, joints: &[usize], times: &[f32]) -> Vec<Vec<Vec3>> {
    let mut out = vec![Vec::new(); joints.len()];
    for &t in times {
        let mut pose = skeleton.bind_pose_pose();
        clip.sample_pose(t, &mut pose);
        let globals = skeleton.globals(&pose, None);
        for (slot, &j) in out.iter_mut().zip(joints) {
            slot.push(globals[j].w_axis.truncate());
        }
    }
    out
}

/// Planted runs of a source foot: the motion-matching contact labels over the frames.
fn planted_runs(foot: &[Vec3], times: &[f32]) -> Vec<Range<usize>> {
    let lowest = foot.iter().map(|p| p.y).fold(f32::INFINITY, f32::min);
    let heights: Vec<f32> = foot.iter().map(|p| p.y - lowest).collect();
    let n = foot.len();
    let speeds: Vec<f32> = (0..n)
        .map(|i| {
            let (a, b) = (i.saturating_sub(1), (i + 1).min(n - 1));
            (foot[b] - foot[a]).length() / (times[b] - times[a])
        })
        .collect();
    let labels = label_contacts(&heights, &speeds, false, &ContactSettings::default());
    let mut runs = Vec::new();
    let mut start = None;
    for (i, &planted) in labels.iter().chain(std::iter::once(&false)).enumerate() {
        match (planted, start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                runs.push(s..i);
                start = None;
            }
            _ => {}
        }
    }
    runs
}

/// The largest horizontal distance a target foot moves from where it stood at the start of each
/// planted run.
fn slide(target_foot: &[Vec3], runs: &[Range<usize>]) -> f32 {
    let flat = |v: Vec3| Vec3::new(v.x, 0.0, v.z);
    runs.iter()
        .flat_map(|run| {
            let start = target_foot[run.start];
            target_foot[run.clone()]
                .iter()
                .map(move |p| flat(*p - start).length())
        })
        .fold(0.0, f32::max)
}

/// Slide of both feet and the worst planted height error against the target's ankle height.
fn walk_slide(goals: Vec<IkGoalSpec>) -> (f32, f32) {
    let (source, target) = (rig(&SOURCE), rig(&TARGET));
    walk_slide_with(&retargeter(&source, &target, goals))
}

fn walk_slide_with(rt: &Retargeter) -> (f32, f32) {
    let (source, target) = (rt.source(), rt.target());
    let clip = walk(source);
    let baked = rt.retarget_clip(&clip);
    let times = key_times(&baked);
    let src = track(source, &clip, &[14, 18], &times);
    let tgt = track(target, &baked, &[14, 18], &times);
    let mut worst = 0.0f32;
    let mut height = 0.0f32;
    for side in 0..2 {
        let runs = planted_runs(&src[side], &times);
        assert!(!runs.is_empty(), "the demo walk plants every foot");
        worst = worst.max(slide(&tgt[side], &runs));
        for run in &runs {
            for p in &tgt[side][run.clone()] {
                height = height.max((p.y - TARGET.ankle).abs());
            }
        }
    }
    (worst, height)
}

#[test]
fn long_leg_retarget_keeps_planted_feet_within_1cm() {
    let (control, _) = walk_slide(Vec::new());
    let (held, height) = walk_slide(feet_goals(IkGoalMode::Contacts));
    eprintln!(
        "planted slide: {control:.4} m without goals, {held:.4} m with Contacts goals; height error {height:.4} m"
    );
    assert!(
        control > 0.03,
        "control: without goals the long-leg feet slide ({control} m)"
    );
    assert!(held <= 0.01, "Contacts goals hold the planted feet ({held} m)");
    assert!(
        height <= 0.01,
        "planted feet stand at the target's ankle height ({height} m off)"
    );
}

#[test]
fn retarget_check_reports_goal_residual_and_foot_slide() {
    let (source, target) = (rig(&SOURCE), rig(&TARGET));
    let clip = walk(&source);
    let rt = retargeter(&source, &target, feet_goals(IkGoalMode::Contacts));
    let report = rt.check(Some(&clip));
    let check = report.clip.as_ref().expect("clip part");
    assert_eq!(check.goals.len(), 2, "{:?}", check.goals);
    for goal in &check.goals {
        assert!(goal.resolved, "{goal:?}");
        assert!(goal.max_residual <= 0.005, "{goal:?}");
        let slide = goal.planted_slide.expect("contacts goals report slide");
        assert!(slide <= 0.01, "{goal:?}");
    }
    assert!(
        !report.issues.iter().any(|i| i.code.starts_with("goal_")),
        "{:?}",
        report.issues
    );
    // Without goals the same check reports the slide through `foot_skate` only; the goal list is
    // empty.
    let plain = retargeter(&source, &target, Vec::new()).check(Some(&clip));
    assert!(plain.clip.as_ref().expect("clip").goals.is_empty());
    // A goal naming a role the rigs do not pair is reported, not an error.
    let rt = retargeter(
        &source,
        &target,
        vec![IkGoalSpec::new(
            ["leftUpperLeg", "leftLowerLeg", "tailTip"],
            IkGoalMode::Contacts,
        )],
    );
    let report = rt.check(Some(&clip));
    assert!(
        report.issues.iter().any(|i| i.code == "goal_unmapped"),
        "{:?}",
        report.issues
    );
    assert!(!report.clip.as_ref().expect("clip").goals[0].resolved);
}

/// A two-handed hold: both hands on the ends of a bar that moves and turns in front of the chest.
fn hold(source: &Skeleton) -> AnimationClip {
    let frames = 30usize;
    let duration = 1.0f32;
    let times: Vec<f32> = (0..=frames)
        .map(|k| k as f32 / frames as f32 * duration)
        .collect();
    let mut rotations: [Vec<Quat>; 4] = Default::default();
    for &t in &times {
        let phase = std::f32::consts::TAU * t / duration;
        let center = Vec3::new(0.05 * phase.sin(), 1.2 + 0.1 * (2.0 * phase).sin(), -0.3);
        let half = Quat::from_rotation_y(0.3 * phase.sin()) * Vec3::new(0.18, 0.0, 0.0);
        let mut pose = source.bind_pose_pose();
        let left = IkChain {
            root: 6,
            mid: 7,
            tip: 8,
        };
        let right = IkChain {
            root: 9,
            mid: 10,
            tip: 11,
        };
        solve_two_bone(
            source,
            &mut pose,
            &left,
            center - half,
            Some(Vec3::new(-0.5, 0.9, 0.0)),
            1.0,
        );
        solve_two_bone(
            source,
            &mut pose,
            &right,
            center + half,
            Some(Vec3::new(0.5, 0.9, 0.0)),
            1.0,
        );
        for (k, j) in [6usize, 7, 9, 10].into_iter().enumerate() {
            rotations[k].push(pose.rotations[j]);
        }
    }
    let mut clip = AnimationClip::empty("hold".to_string(), duration);
    clip.sample_rate = frames as f32 / duration;
    for (k, j) in [6usize, 7, 9, 10].into_iter().enumerate() {
        let bind = source.bind_pose[j];
        clip.tracks.push(JointTrack {
            joint: j as u32,
            translations: TrackCurve::Static(bind.translation),
            rotations: TrackCurve::Animated {
                times: times.clone(),
                values: std::mem::take(&mut rotations[k]),
                exact_rotation: false,
            },
            scales: TrackCurve::Static(bind.scale),
        });
    }
    clip
}

/// The worst difference between the target's hand gap and the scaled source gap.
fn gap_error(goals: Vec<IkGoalSpec>) -> f32 {
    let (source, target) = (rig(&SOURCE), rig(&TARGET));
    let clip = hold(&source);
    let rt = retargeter(&source, &target, goals);
    let baked = rt.retarget_clip(&clip);
    let times = key_times(&baked);
    let src = track(&source, &clip, &[8, 11], &times);
    let tgt = track(&target, &baked, &[8, 11], &times);
    (0..times.len())
        .map(|i| {
            let want = (src[1][i] - src[0][i]) * rt.scale();
            let got = tgt[1][i] - tgt[0][i];
            (got - want).length()
        })
        .fold(0.0, f32::max)
}

#[test]
fn hands_on_a_prop_keep_their_gap() {
    let control = gap_error(Vec::new());
    let held = gap_error(vec![
        IkGoalSpec::new(
            ["leftUpperArm", "leftLowerArm", "leftHand"],
            IkGoalMode::ScaledWorld,
        ),
        IkGoalSpec::new(
            ["rightUpperArm", "rightLowerArm", "rightHand"],
            IkGoalMode::ScaledWorld,
        ),
    ]);
    eprintln!("hand gap error: {control:.4} m without goals, {held:.4} m with ScaledWorld goals");
    assert!(
        control > 0.03,
        "control: the target's arms change the gap ({control} m)"
    );
    assert!(held <= 0.01, "ScaledWorld goals keep the gap ({held} m)");
}

fn fnv(hash: &mut u64, bits: u32) {
    for byte in bits.to_le_bytes() {
        *hash ^= u64::from(byte);
        *hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
}

/// Every key of a baked clip, sampled at its key times, plus its root-motion joint.
fn bake_digest(baked: &[AnimationClip], skeletons: &[Skeleton]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for (clip, skeleton) in baked.iter().zip(skeletons) {
        for t in key_times(clip) {
            let mut pose = skeleton.bind_pose_pose();
            clip.sample_pose(t, &mut pose);
            for j in 0..pose.len() {
                for v in pose.translations[j].to_array() {
                    fnv(&mut hash, v.to_bits());
                }
                for v in pose.rotations[j].to_array() {
                    fnv(&mut hash, v.to_bits());
                }
                for v in pose.scales[j].to_array() {
                    fnv(&mut hash, v.to_bits());
                }
            }
        }
        fnv(
            &mut hash,
            clip.root_motion.as_ref().map_or(u32::MAX, |rm| rm.joint),
        );
        fnv(&mut hash, clip.tracks.len() as u32);
    }
    hash
}

/// Bakes the long-leg walk, the hold and a demo walk onto a rolled, game-named demo rig.
fn bakes(options: &RetargetOptions) -> (Vec<AnimationClip>, Vec<Skeleton>) {
    let (source, target) = (rig(&SOURCE), rig(&TARGET));
    let pair = |s: &Skeleton, t: &Skeleton| {
        Retargeter::with_options(
            s,
            &RetargetProfile::guess_humanoid(s),
            t,
            &RetargetProfile::guess_humanoid(t),
            options,
        )
        .expect("pairs")
    };
    let rt = pair(&source, &target);
    let demo = demo_humanoid(&DemoRig::default()).expect("demo");
    let game = demo_humanoid(&DemoRig {
        naming: RigNaming::Game,
        size: 1.2,
        rolled_frames: true,
        ..DemoRig::default()
    })
    .expect("game rig");
    let demo_walk = demo_locomotion(&demo, &DemoGait::standard()).remove(1);
    let baked = vec![
        rt.retarget_clip(&walk(&source)),
        rt.retarget_clip(&hold(&source)),
        pair(&demo, &game).retarget_clip(&demo_walk),
    ];
    (baked, vec![target.clone(), target, game])
}

/// The digest of [`bakes`] recorded on the base before IK goals existed (9951ecaba, x86_64; the
/// lane report has the run).
const BASE_BAKE_DIGEST: u64 = 0x12f9_aa95_6ce5_fc33;

#[test]
fn ik_goals_off_is_bit_identical_to_the_current_bake() {
    let options = RetargetOptions {
        ik_goals: Vec::new(),
        ..RetargetOptions::default()
    };
    let (baked, skeletons) = bakes(&options);
    let digest = bake_digest(&baked, &skeletons);
    let (again, _) = bakes(&RetargetOptions::default());
    assert_eq!(
        digest,
        bake_digest(&again, &skeletons),
        "default options carry no goals"
    );
    eprintln!("bake digest {digest:#018x}");
    if cfg!(target_arch = "x86_64") {
        assert_eq!(digest, BASE_BAKE_DIGEST, "no goals bakes what the base baked");
    }
    // Goals with weight 0 change nothing either.
    let mut zero = feet_goals(IkGoalMode::Contacts);
    for goal in &mut zero {
        goal.weight = 0.0;
    }
    let (weightless, _) = bakes(&RetargetOptions {
        ik_goals: zero,
        ..RetargetOptions::default()
    });
    assert_eq!(bake_digest(&weightless, &skeletons), digest);
}

const V1: &str = r#"(
    format_version: 1,
    name: "mixamo-to-guard",
    source: (roles: Guess),
    target: (roles: Profile("generic-humanoid")),
    roles: { "hips": (translation: Some(Scaled)) },
    translation_scale: Fixed(1.5),
)"#;

#[test]
fn retarget_ron_v1_loads_as_v2() {
    assert_eq!(RETARGET_FORMAT_VERSION, 2);
    let setup = RetargetSetup::from_ron_str(V1).expect("v1 loads");
    assert_eq!(setup.format_version, 2);
    assert!(setup.ik_goals.is_empty(), "v1 has no goals");
    assert_eq!(setup.name, "mixamo-to-guard");
    let migrated = migrate_retarget(V1).expect("migrates");
    assert!(migrated.contains("format_version: 2"), "{migrated}");
    assert_eq!(RetargetSetup::from_ron_str(&migrated).expect("reloads"), setup);
    // A v2 document with goals round-trips.
    let v2 = r#"(
        format_version: 2,
        ik_goals: [
            (roles: ("leftUpperLeg", "leftLowerLeg", "leftFoot"), mode: Contacts),
            (roles: ("rightUpperArm", "rightLowerArm", "rightHand"), mode: ScaledWorld, weight: 0.5),
        ],
    )"#;
    let setup = RetargetSetup::from_ron_str(v2).expect("v2 loads");
    assert_eq!(setup.ik_goals.len(), 2);
    assert_eq!(setup.ik_goals[0].mode, IkGoalMode::Contacts);
    assert_eq!(setup.ik_goals[0].weight, 1.0, "weight defaults to 1");
    assert_eq!(setup.ik_goals[1].weight, 0.5);
    assert_eq!(setup.options().ik_goals, setup.ik_goals);
    let text = setup.to_ron_string().expect("writes");
    assert_eq!(RetargetSetup::from_ron_str(&text).expect("reparses"), setup);
}

#[test]
fn v3_is_refused() {
    let v3 = V1.replace("format_version: 1", "format_version: 3");
    let err = RetargetSetup::from_ron_str(&v3).expect_err("v3 refused");
    assert_eq!(err.code(), "bad_setup");
    assert!(err.to_string().contains('3'), "{err}");
    assert!(migrate_retarget(&v3).is_err());
    // Bad goal values are refused too.
    for bad in [
        "(format_version: 2, ik_goals: [(roles: (\"a\", \"b\", \"c\"), weight: 2.0)])",
        "(format_version: 2, ik_goals: [(roles: (\"a\", \"b\", \"\"))])",
        "(format_version: 2, ik_goals: [(roles: (\"a\", \"a\", \"c\"))])",
    ] {
        let err = RetargetSetup::from_ron_str(bad).expect_err(bad);
        assert_eq!(err.code(), "bad_setup", "{bad}");
    }
}

#[test]
fn custom_role_goals_and_repeat_bakes() {
    // Custom roles: both rigs paired by joint name, the goals naming joints, not humanoid roles.
    let (source, target) = (rig(&SOURCE), rig(&TARGET));
    let options = RetargetOptions {
        ik_goals: vec![
            IkGoalSpec::new(["left_thigh", "left_calf", "left_foot"], IkGoalMode::Contacts),
            IkGoalSpec::new(["right_thigh", "right_calf", "right_foot"], IkGoalMode::Contacts),
        ],
        ..RetargetOptions::default()
    };
    let rt = Retargeter::with_options(
        &source,
        &RetargetProfile::by_name(&source),
        &target,
        &RetargetProfile::by_name(&target),
        &options,
    )
    .expect("pairs by name");
    assert_eq!(rt.ik_goals().count(), 2);
    let (held, height) = walk_slide_with(&rt);
    assert!(held <= 0.01, "custom-role goals hold the feet ({held} m)");
    assert!(height <= 0.01, "{height}");
    // The same inputs bake the same bits.
    let clip = walk(&source);
    let (a, b) = (rt.retarget_clip(&clip), rt.retarget_clip(&clip));
    assert_eq!(
        bake_digest(std::slice::from_ref(&a), std::slice::from_ref(&target)),
        bake_digest(std::slice::from_ref(&b), std::slice::from_ref(&target))
    );
}
