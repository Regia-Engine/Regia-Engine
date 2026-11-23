// SPDX-License-Identifier: MPL-2.0
//! Retargeting on real-world rig conventions (`docs/specs/85-retarget-crowds.md` § 2, § 6):
//! exporter naming, centimetre rigs, rolled bind frames, opposite facing, per-role settings and
//! the quality report.

use std::collections::BTreeMap;

use regia_animation::demo_rig::{
    DemoGait, DemoRig, RigNaming, demo_humanoid, demo_in_place, demo_locomotion, demo_travelling_hips,
};
use regia_animation::retarget::{RetargetProfile, Retargeter};
use regia_animation::skeleton::LocalBind;
use regia_animation::{
    AnimationClip, RetargetOptions, RetargetSetup, RigRoles, RoleSettings, RoleSource, RootMotionScale,
    ScaleRule, Skeleton, TranslationMode,
};
use regia_core::math::Vec3;

/// Expected role → canonical demo joint index.
const ROLES: [(&str, usize); 19] = [
    ("hips", 1),
    ("spine", 2),
    ("chest", 3),
    ("neck", 4),
    ("head", 5),
    ("leftUpperArm", 6),
    ("leftLowerArm", 7),
    ("leftHand", 8),
    ("rightUpperArm", 9),
    ("rightLowerArm", 10),
    ("rightHand", 11),
    ("leftUpperLeg", 12),
    ("leftLowerLeg", 13),
    ("leftFoot", 14),
    ("leftToes", 15),
    ("rightUpperLeg", 16),
    ("rightLowerLeg", 17),
    ("rightFoot", 18),
    ("rightToes", 19),
];

fn rig(rig: DemoRig) -> Skeleton {
    demo_humanoid(&rig).expect("demo rig")
}

fn source() -> (Skeleton, Vec<AnimationClip>) {
    let sk = rig(DemoRig::default());
    let clips = demo_locomotion(&sk, &DemoGait::standard());
    (sk, clips)
}

#[test]
fn exporter_namings_guess_every_humanoid_role_and_skip_twist_ik_and_end_joints() {
    for naming in [
        RigNaming::Mixamo,
        RigNaming::Game,
        RigNaming::Biped,
        RigNaming::Plain,
    ] {
        let sk = rig(DemoRig {
            naming,
            extra_joints: true,
            ..DemoRig::default()
        });
        let profile = RetargetProfile::guess_humanoid(&sk);
        for (role, joint) in ROLES {
            assert_eq!(
                profile.roles.get(role).map(String::as_str),
                Some(sk.joint_names[joint].as_str()),
                "{naming:?}: role {role}"
            );
        }
        for extra in &sk.joint_names[20..] {
            assert!(
                !profile.roles.values().any(|j| j == extra),
                "{naming:?}: {extra} must not take a role"
            );
        }
        assert!(profile.missing_humanoid_roles().is_empty(), "{naming:?}");
    }
}

/// A 20-joint rig with the canonical hierarchy and the given names (bind positions from the
/// plain demo rig).
fn renamed(names: [&str; 20]) -> Skeleton {
    let base = rig(DemoRig::default());
    Skeleton::from_local_bind(
        names.iter().map(|n| n.to_string()).collect(),
        base.parents.clone(),
        base.bind_pose.clone(),
    )
    .expect("skeleton")
}

#[test]
fn camel_case_prefixed_and_deform_namings_guess_the_limbs() {
    // Figure-style camel case with an `l`/`r` prefix and `Bend` segments.
    let figure = renamed([
        "Genesis8Male",
        "hip",
        "abdomenLower",
        "chestUpper",
        "neckLower",
        "head",
        "lShldrBend",
        "lForearmBend",
        "lHand",
        "rShldrBend",
        "rForearmBend",
        "rHand",
        "lThighBend",
        "lShin",
        "lFoot",
        "lToe",
        "rThighBend",
        "rShin",
        "rFoot",
        "rToe",
    ]);
    // Deform-bone prefixes with `.L`/`.R` suffixes.
    let deform = renamed([
        "root",
        "DEF-pelvis",
        "DEF-spine.001",
        "DEF-chest",
        "DEF-neck",
        "DEF-head",
        "DEF-upper_arm.L",
        "DEF-forearm.L",
        "DEF-hand.L",
        "DEF-upper_arm.R",
        "DEF-forearm.R",
        "DEF-hand.R",
        "DEF-thigh.L",
        "DEF-shin.L",
        "DEF-foot.L",
        "DEF-toe.L",
        "DEF-thigh.R",
        "DEF-shin.R",
        "DEF-foot.R",
        "DEF-toe.R",
    ]);
    for sk in [figure, deform] {
        let profile = RetargetProfile::guess_humanoid(&sk);
        for (role, joint) in ROLES {
            assert_eq!(
                profile.roles.get(role).map(String::as_str),
                Some(sk.joint_names[joint].as_str()),
                "{}: role {role}",
                sk.joint_names[1]
            );
        }
    }
}

/// The joint names and hierarchy of the Khronos glTF sample "CesiumMan" (glTF-Sample-Assets,
/// CC BY 4.0, Cesium), parents listed first. Bind transforms are not needed by the guesser.
fn cesium_man_hierarchy() -> Skeleton {
    let joints: [(&str, Option<u32>); 19] = [
        ("Skeleton_torso_joint_1", None),
        ("Skeleton_torso_joint_2", Some(0)),
        ("torso_joint_3", Some(1)),
        ("Skeleton_neck_joint_1", Some(2)),
        ("Skeleton_neck_joint_2", Some(3)),
        ("Skeleton_arm_joint_L__4_", Some(2)),
        ("Skeleton_arm_joint_R", Some(2)),
        ("Skeleton_arm_joint_L__3_", Some(5)),
        ("Skeleton_arm_joint_R__2_", Some(6)),
        ("Skeleton_arm_joint_L__2_", Some(7)),
        ("Skeleton_arm_joint_R__3_", Some(8)),
        ("leg_joint_L_1", Some(0)),
        ("leg_joint_R_1", Some(0)),
        ("leg_joint_L_2", Some(11)),
        ("leg_joint_R_2", Some(12)),
        ("leg_joint_L_3", Some(13)),
        ("leg_joint_R_3", Some(14)),
        ("leg_joint_L_5", Some(15)),
        ("leg_joint_R_5", Some(16)),
    ];
    Skeleton::from_local_bind(
        joints.iter().map(|(n, _)| n.to_string()).collect(),
        joints.iter().map(|(_, p)| *p).collect(),
        vec![LocalBind::default(); joints.len()],
    )
    .expect("skeleton")
}

#[test]
fn generic_chain_names_of_a_real_sample_rig_fill_every_role() {
    let sk = cesium_man_hierarchy();
    let profile = RetargetProfile::guess_humanoid(&sk);
    let expect = [
        ("hips", "Skeleton_torso_joint_1"),
        ("spine", "Skeleton_torso_joint_2"),
        ("chest", "torso_joint_3"),
        ("neck", "Skeleton_neck_joint_1"),
        ("head", "Skeleton_neck_joint_2"),
        ("leftUpperArm", "Skeleton_arm_joint_L__4_"),
        ("leftLowerArm", "Skeleton_arm_joint_L__3_"),
        ("leftHand", "Skeleton_arm_joint_L__2_"),
        ("rightUpperArm", "Skeleton_arm_joint_R"),
        ("rightLowerArm", "Skeleton_arm_joint_R__2_"),
        ("rightHand", "Skeleton_arm_joint_R__3_"),
        ("leftUpperLeg", "leg_joint_L_1"),
        ("leftLowerLeg", "leg_joint_L_2"),
        ("leftFoot", "leg_joint_L_3"),
        ("leftToes", "leg_joint_L_5"),
        ("rightUpperLeg", "leg_joint_R_1"),
        ("rightLowerLeg", "leg_joint_R_2"),
        ("rightFoot", "leg_joint_R_3"),
        ("rightToes", "leg_joint_R_5"),
    ];
    for (role, joint) in expect {
        assert_eq!(
            profile.roles.get(role).map(String::as_str),
            Some(joint),
            "role {role}"
        );
    }
    assert!(profile.missing_humanoid_roles().is_empty());
    assert!(
        !profile.roles.contains_key("leftShoulder"),
        "three arm joints: no shoulder"
    );
}

#[test]
fn opposite_facing_centimetre_rolled_rig_walks_forward_without_skating() {
    let (source, clips) = source();
    let target = rig(DemoRig {
        naming: RigNaming::Game,
        size: 0.9,
        facing_positive_z: true,
        centimetres: true,
        rolled_frames: true,
        extra_joints: true,
        ..DemoRig::default()
    });
    let rt = Retargeter::humanoid(&source, &target).expect("retargeter");
    assert!(
        (rt.facing_yaw().abs() - std::f32::consts::PI).abs() < 1e-3,
        "{}",
        rt.facing_yaw()
    );
    assert!((rt.scale() - 0.9).abs() < 1e-3, "{}", rt.scale());
    for clip in &clips {
        let report = rt.check(Some(clip));
        let issues: Vec<_> = report.issues.iter().map(|i| i.code).collect();
        assert_eq!(issues, ["facing_turned"], "{}: {:?}", clip.name, report.issues);
        assert!(report.ok());
        let check = report.clip.as_ref().expect("clip check");
        assert!(check.max_direction_error_deg < 0.1, "{}: {check:?}", clip.name);
        if let Some(skate) = check.target_foot_skate {
            assert!(skate < 0.05, "{}: target skates {skate}", clip.name);
        }
        if clip.root_motion.is_some() {
            let ratio = check.speed_ratio.expect("speed ratio");
            assert!((ratio - 0.9).abs() < 0.01, "{}: {ratio}", clip.name);
        }
    }
    // The walk travels along the target's forward (+Z), 0.9 × 1.3 m per lap.
    let baked = rt.retarget_clip(&clips[1]);
    let (delta, _) = baked
        .root_motion_delta_full(0.0, baked.duration, false)
        .expect("root motion");
    assert!((delta - Vec3::new(0.0, 0.0, 1.17)).length() < 0.01, "{delta}");
    // Without facing alignment the same rig walks backwards relative to its legs: bone
    // directions disagree with the turned source.
    let options = RetargetOptions {
        align_facing: false,
        ..RetargetOptions::default()
    };
    let unaligned = Retargeter::with_options(
        &source,
        &RetargetProfile::guess_humanoid(&source),
        &target,
        &RetargetProfile::guess_humanoid(&target),
        &options,
    )
    .expect("unaligned");
    assert_eq!(unaligned.facing_yaw(), 0.0);
}

#[test]
fn a_pose_biped_from_a_rolled_centimetre_source_keeps_bone_directions() {
    let plain = rig(DemoRig::default());
    let clips = demo_locomotion(&plain, &DemoGait::standard());
    let source = rig(DemoRig {
        naming: RigNaming::Mixamo,
        centimetres: true,
        rolled_frames: true,
        ..DemoRig::default()
    });
    // The clips were authored on the plain rig: move them onto the Mixamo rig first.
    let to_source = Retargeter::humanoid(&plain, &source).expect("plain → mixamo");
    let walk = to_source.retarget_clip(&clips[1]);
    let target = rig(DemoRig {
        naming: RigNaming::Biped,
        size: 1.1,
        a_pose: true,
        ..DemoRig::default()
    });
    let rt = Retargeter::humanoid(&source, &target).expect("mixamo → biped");
    let report = rt.check(Some(&walk));
    assert!(report.ok(), "{:?}", report.issues);
    assert!(report.facing_yaw_deg.abs() < 1e-3);
    let check = report.clip.expect("clip");
    assert!(check.max_direction_error_deg < 0.1, "{check:?}");
    assert!(
        (check.speed_ratio.expect("ratio") - 1.1).abs() < 0.01,
        "{check:?}"
    );
    // The A-pose arms need a visible rest correction; the legs need none.
    let correction = |role: &str| {
        report
            .mapped
            .iter()
            .find(|m| m.role == role)
            .map(|m| m.rest_correction_deg)
            .expect("mapped")
    };
    assert!(
        (correction("leftUpperArm") - 40.0).abs() < 0.5,
        "{}",
        correction("leftUpperArm")
    );
    assert!(correction("leftUpperLeg") < 0.1);
}

fn options_with(role: &str, settings: RoleSettings) -> RetargetOptions {
    let mut roles = BTreeMap::new();
    roles.insert(role.to_string(), settings);
    RetargetOptions {
        roles,
        ..RetargetOptions::default()
    }
}

fn build(source: &Skeleton, target: &Skeleton, options: &RetargetOptions) -> Retargeter {
    Retargeter::with_options(
        source,
        &RetargetProfile::guess_humanoid(source),
        target,
        &RetargetProfile::guess_humanoid(target),
        options,
    )
    .expect("retargeter")
}

#[test]
fn rest_offsets_translation_modes_skips_and_root_motion_rules_apply() {
    let (source, clips) = source();
    let walk = &clips[1];
    let target = rig(DemoRig {
        naming: RigNaming::Game,
        size: 0.8,
        ..DemoRig::default()
    });

    // A 20° rest offset on the left thigh turns exactly that bone.
    let offset = build(
        &source,
        &target,
        &options_with(
            "leftUpperLeg",
            RoleSettings {
                rest_offset_deg: Vec3::new(20.0, 0.0, 0.0),
                ..RoleSettings::default()
            },
        ),
    );
    let report = offset.check(Some(walk));
    let check = report.clip.as_ref().expect("clip");
    assert_eq!(check.worst_role.as_deref(), Some("leftUpperLeg"));
    assert!((check.max_direction_error_deg - 20.0).abs() < 1.0, "{check:?}");
    assert!(report.issues.iter().any(|i| i.code == "direction_error"));

    // Animated hips copy the source's local translation; Bind keeps the target's.
    let mut pose = source.bind_pose_pose();
    walk.sample_pose(0.3, &mut pose);
    let mut out = target.bind_pose_pose();
    for (mode, want) in [
        (TranslationMode::Animated, pose.translations[1]),
        (TranslationMode::Bind, target.bind_pose[1].translation),
    ] {
        let rt = build(
            &source,
            &target,
            &options_with(
                "hips",
                RoleSettings {
                    translation: Some(mode),
                    ..RoleSettings::default()
                },
            ),
        );
        rt.retarget_pose(&pose, &mut out);
        assert!((out.translations[1] - want).length() < 1e-5, "{mode:?}");
    }

    // Skipped roles stay unmapped.
    let skip = build(
        &source,
        &target,
        &options_with(
            "leftToes",
            RoleSettings {
                skip: true,
                ..RoleSettings::default()
            },
        ),
    );
    assert_eq!(skip.source_of(15), None);
    assert!(skip.check(None).mapped.iter().all(|m| m.role != "leftToes"));

    // Root motion at twice the translation scale makes the planted feet skate.
    let doubled = RetargetOptions {
        root_motion: RootMotionScale::Fixed(1.6),
        ..RetargetOptions::default()
    };
    let rt = build(&source, &target, &doubled);
    let report = rt.check(Some(walk));
    let codes: Vec<_> = report.issues.iter().map(|i| i.code).collect();
    assert!(codes.contains(&"foot_skate"), "{codes:?}");
    assert!(!codes.contains(&"speed_ratio"), "{codes:?}");
    let ratio = report.clip.and_then(|c| c.speed_ratio).expect("ratio");
    assert!((ratio - 1.6).abs() < 0.02, "{ratio}");

    // Off drops root motion without a `root_motion_lost` issue.
    let off = RetargetOptions {
        root_motion: RootMotionScale::Off,
        ..RetargetOptions::default()
    };
    let rt = build(&source, &target, &off);
    assert!(rt.retarget_clip(walk).root_motion.is_none());
    assert_eq!(rt.root_motion_scale(), None);
    assert!(
        rt.check(Some(walk))
            .issues
            .iter()
            .all(|i| i.code != "root_motion_lost")
    );
}

#[test]
fn a_swapped_arm_mapping_is_reported() {
    let (source, clips) = source();
    let target = rig(DemoRig::default());
    let mut swapped = RetargetProfile::guess_humanoid(&target);
    for (a, b) in [
        ("leftUpperArm", "rightUpperArm"),
        ("leftLowerArm", "rightLowerArm"),
        ("leftHand", "rightHand"),
    ] {
        let (ja, jb) = (swapped.roles[a].clone(), swapped.roles[b].clone());
        swapped.roles.insert(a.into(), jb);
        swapped.roles.insert(b.into(), ja);
    }
    let rt = Retargeter::new(
        &source,
        &RetargetProfile::guess_humanoid(&source),
        &target,
        &swapped,
    )
    .expect("retargeter");
    let report = rt.check(Some(&clips[1]));
    assert!(!report.ok());
    assert!(
        report.issues.iter().any(|i| i.code == "large_rest_correction"),
        "{:?}",
        report.issues
    );
    // A correct mapping of the same rigs is clean.
    let clean = Retargeter::humanoid(&source, &target).expect("clean");
    assert!(clean.check(Some(&clips[1])).issues.is_empty());
}

#[test]
fn setups_build_from_profiles_explicit_maps_and_overrides() {
    let source = rig(DemoRig {
        naming: RigNaming::Mixamo,
        ..DemoRig::default()
    });
    let target = rig(DemoRig {
        naming: RigNaming::Game,
        ..DemoRig::default()
    });
    let setup = RetargetSetup {
        name: "mixamo-to-game".into(),
        source: RigRoles {
            roles: RoleSource::Profile("generic-humanoid".into()),
            overrides: BTreeMap::new(),
        },
        target: RigRoles {
            roles: RoleSource::Guess,
            overrides: [("leftToes".to_string(), "ball_l".to_string())].into(),
        },
        ..RetargetSetup::default()
    };
    let rt = setup.build(&source, &target).expect("builds");
    assert!(rt.mapped_joints() >= 19, "{}", rt.mapped_joints());
    // Through RON and back.
    let text = setup.to_ron_string().expect("writes");
    let again = RetargetSetup::from_ron_str(&text).expect("reads");
    assert_eq!(again, setup);
    // An explicit map naming an absent joint fails with its code.
    let bad = RetargetSetup {
        target: RigRoles {
            roles: RoleSource::Explicit([("hips".to_string(), "nope".to_string())].into()),
            overrides: BTreeMap::new(),
        },
        ..setup.clone()
    };
    assert_eq!(
        bad.build(&source, &target).err().map(|e| e.code()),
        Some("missing_joint")
    );
    let none = RetargetSetup {
        target: RigRoles {
            roles: RoleSource::Explicit([("tail".to_string(), "root".to_string())].into()),
            overrides: BTreeMap::new(),
        },
        ..setup
    };
    assert_eq!(
        none.build(&source, &target).err().map(|e| e.code()),
        Some("no_common_roles")
    );
}

/// Imported clips usually have no root-motion track: the report then measures a treadmill clip
/// by its stance feet, and the skate over the ground instead of against a still foot.
#[test]
fn in_place_clips_are_checked_against_their_stance_feet() {
    let (source, clips) = source();
    let walk = demo_in_place(&clips[1]);
    let target = rig(DemoRig {
        naming: RigNaming::Game,
        size: 0.8,
        facing_positive_z: true,
        rolled_frames: true,
        ..DemoRig::default()
    });
    let rt = Retargeter::humanoid(&source, &target).expect("retargeter");
    let report = rt.check(Some(&walk));
    assert!(report.ok(), "{:?}", report.issues);
    let check = report.clip.expect("clip");
    assert_eq!(check.source_root_speed, None);
    let speed = check.source_stance_speed.expect("stance speed");
    assert!((speed - 1.3).abs() < 0.04, "{speed}");
    let baked = check.target_stance_speed.expect("baked stance speed");
    assert!((baked - 1.3 * rt.scale()).abs() < 0.04, "{baked}");
    let ratio = check.speed_ratio.expect("ratio");
    assert!((ratio - rt.scale()).abs() < 0.02, "{ratio}");
    let skate = check.source_foot_skate.expect("planted frames found");
    assert!(skate < 0.05, "source skate {skate}");
    let skate = check.target_foot_skate.expect("target skate");
    assert!(skate < 0.05, "target skate {skate}");

    // An idle and a clip whose travel is on the hips have no stance speed: their feet hold.
    for clip in [&clips[0], &demo_travelling_hips(&clips[1])] {
        let report = rt.check(Some(clip));
        let check = report.clip.expect("clip");
        assert_eq!(check.source_stance_speed, None, "{}", check.name);
        assert_eq!(check.target_stance_speed, None, "{}", check.name);
    }

    // The stance ratio follows the legs, so a translation scale that disagrees with them is no
    // treadmill issue (CesiumMan's stride ratio is 9 % above its hip-height ratio).
    assert!((rt.check(None).leg_ratio.expect("legs") - 0.8).abs() < 1e-3);
    let doubled = RetargetOptions {
        translation_scale: ScaleRule::Fixed(2.0),
        ..RetargetOptions::default()
    };
    let report = build(&source, &target, &doubled).check(Some(&walk));
    assert!(
        report.issues.iter().all(|i| i.code != "speed_ratio"),
        "{:?}",
        report.issues
    );
    // Legs that do not follow the source are what `speed_ratio` is for here.
    let stiff = RoleSettings {
        skip: true,
        ..RoleSettings::default()
    };
    let mut options = options_with("leftUpperLeg", stiff.clone());
    options.roles.insert("rightUpperLeg".into(), stiff);
    let report = build(&source, &target, &options).check(Some(&walk));
    assert!(
        report.issues.iter().any(|i| i.code == "speed_ratio"),
        "{:?}",
        report.issues
    );
}

/// Spec 85 § 2.4 (rig part): the rest corrections are listed per role, largest first (ties by
/// role name), so the first entry is the joint that needed the most correction.
#[test]
fn rest_corrections_are_listed_largest_first() {
    let source = rig(DemoRig::default());
    let target = rig(DemoRig {
        naming: RigNaming::Biped,
        a_pose: true,
        ..DemoRig::default()
    });
    let report = Retargeter::humanoid(&source, &target)
        .expect("plain → A-pose biped")
        .check(None);
    assert!(report.mapped.len() >= 19, "{}", report.mapped.len());
    assert!(
        report.mapped[0].rest_correction_deg > 1.0,
        "the A-pose arms lead the list: {:?}",
        report.mapped[0]
    );
    for pair in report.mapped.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        assert!(
            a.rest_correction_deg > b.rest_correction_deg
                || (a.rest_correction_deg == b.rest_correction_deg && a.role < b.role),
            "{a:?} before {b:?}"
        );
    }
}

/// Spec 85 § 2.4 issue `missing_role`: a required humanoid role the target does not map is
/// reported (the hips are mapped, so the rig is a humanoid with a hole).
#[test]
fn a_target_without_a_required_role_reports_missing_role() {
    let source = rig(DemoRig::default());
    let target = rig(DemoRig {
        naming: RigNaming::Game,
        ..DemoRig::default()
    });
    let mut roles = RetargetProfile::guess_humanoid(&target).roles;
    assert!(roles.remove("leftFoot").is_some(), "{roles:?}");
    let setup = RetargetSetup {
        name: "no-left-foot".into(),
        target: RigRoles {
            roles: RoleSource::Explicit(roles),
            overrides: BTreeMap::new(),
        },
        ..RetargetSetup::default()
    };
    let report = setup.build(&source, &target).expect("builds").check(None);
    assert_eq!(report.missing_humanoid_roles, vec!["leftFoot".to_string()]);
    assert!(
        report
            .issues
            .iter()
            .any(|i| i.code == "missing_role" && i.detail.contains("leftFoot")),
        "{:?}",
        report.issues
    );
    assert!(!report.ok());
}

/// Spec 85 § 2.4: a clip is baked at its rate for at most `CHECK_MAX_FRAMES` (600) frames.
#[test]
fn the_clip_check_samples_at_most_600_frames() {
    let (sk, clips) = source();
    let target = rig(DemoRig {
        size: 0.9,
        ..DemoRig::default()
    });
    let rt = Retargeter::humanoid(&sk, &target).expect("retargets");
    let walk = clips[1].clone();
    let normal = rt.check(Some(&walk)).clip.expect("clip");
    let expected = (walk.duration * walk.sample_rate).ceil() as usize + 1;
    assert_eq!(
        normal.frames,
        expected.min(regia_animation::retarget_check::CHECK_MAX_FRAMES)
    );
    let mut dense = walk;
    dense.sample_rate = 10_000.0;
    let capped = rt.check(Some(&dense)).clip.expect("clip");
    assert_eq!(capped.frames, 600);
    assert_eq!(regia_animation::retarget_check::CHECK_MAX_FRAMES, 600);
}

/// Spec 85 § 2.4 issue `root_motion_lost`: the source clip has root motion, the bake keeps a
/// root-motion scale, but the root-motion joint drives no target joint, so the bake plays in
/// place. Here the target's root plays the hips and the source's root plays nothing.
#[test]
fn a_bake_without_the_root_motion_joint_reports_root_motion_lost() {
    let (source, clips) = source();
    let walk = &clips[1];
    assert_eq!(walk.root_motion.as_ref().map(|rm| rm.joint), Some(0));
    let target = rig(DemoRig::default());
    let explicit = |joint: &str| RigRoles {
        roles: RoleSource::Explicit([("hips".to_string(), joint.to_string())].into()),
        overrides: BTreeMap::new(),
    };
    let setup = RetargetSetup {
        name: "root-lost".into(),
        source: explicit(&source.joint_names[1]),
        target: explicit(&target.joint_names[0]),
        translation_scale: ScaleRule::Fixed(1.0),
        ..RetargetSetup::default()
    };
    let rt = setup.build(&source, &target).expect("builds");
    assert!(rt.root_motion_scale().is_some());
    assert!(rt.retarget_clip(walk).root_motion.is_none());
    let report = rt.check(Some(walk));
    assert!(
        report.issues.iter().any(|i| i.code == "root_motion_lost"),
        "{:?}",
        report.issues
    );
    // The guessed retarget pairs the roots and keeps the clip's root motion.
    let guessed = Retargeter::humanoid(&source, &target).expect("retargets");
    assert!(guessed.retarget_clip(walk).root_motion.is_some());
    assert!(
        guessed
            .check(Some(walk))
            .issues
            .iter()
            .all(|i| i.code != "root_motion_lost")
    );
}
