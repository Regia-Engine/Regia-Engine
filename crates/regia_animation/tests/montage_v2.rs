// SPDX-License-Identifier: MPL-2.0
//! Montage format v2 and the warp window extensions (`docs/specs/34-montage-extensions.md`
//! § 4, ADR-1645): v1 files read as v2 with defaults, validation of the new fields, a bone
//! anchor that lands the bone (not the origin) on the target, and a partial rotation window.
//!
//! Every input is fixed (frame times, targets, clips); no wall clock.

use std::sync::Arc;

use regia_animation::clip::{AnimationClip, JointTrack, RootMotionMode, RootMotionTrack, TrackCurve};
use regia_animation::montage::{
    FollowMode, MONTAGE_FORMAT_VERSION, MontageNotify, MontageSection, RateCurve, RateInterpolation,
    SegmentFollow, WarpWindow,
};
use regia_animation::montage_player::{MontageFrame, RigPlacement};
use regia_animation::pose::translated;
use regia_animation::warp::{RotationKind, WarpAnchor, WarpMode, yaw_of};
use regia_animation::{
    ClipLibrary, Montage, MontageError, MontagePlayer, PlayOptions, Skeleton, SlotDef, WarpTarget,
    montage_migration_sources,
};
use regia_core::math::{Quat, Vec3};

const V1: &str = r#"(
    format_version: 1,
    name: "lunge",
    tracks: [(slot: "full_body", segments: [(clip: "dash")])],
    warps: [(target: "enemy", start: 0.2, end: 1.6, rotation: true)],
)"#;

#[test]
fn a_v1_file_reads_as_v2_with_defaults_and_saves_at_v2() {
    assert_eq!(MONTAGE_FORMAT_VERSION, 2);
    assert_eq!(montage_migration_sources(), vec![1]);
    let montage = Montage::from_ron_str(V1).expect("v1 loads");
    assert_eq!(montage.format_version, 2);
    let mut expected = WarpWindow::new("enemy", 0.2, 1.6);
    expected.rotation = true;
    assert_eq!(montage.warps, vec![expected]);
    assert!(montage.rate_curve.is_none());
    let text = montage.to_ron_string().expect("writes");
    assert!(text.contains("format_version: 2"), "{text}");
    assert_eq!(Montage::from_ron_str(&text).expect("round trip"), montage);
}

#[test]
fn a_newer_file_is_refused_by_version() {
    let text = V1.replace("format_version: 1", "format_version: 3");
    assert!(matches!(
        Montage::from_ron_str(&text),
        Err(MontageError::UnknownVersion(3))
    ));
}

#[test]
fn v2_warp_fields_round_trip() {
    let mut m = Montage::single("vault", "full_body", "dash");
    let mut window = WarpWindow::new("ledge", 0.1, 0.9);
    window.mode = WarpMode::PathScale;
    window.rotation_kind = RotationKind::FaceTarget;
    window.rotation_fraction = 0.5;
    window.anchor = WarpAnchor::Bone("hand".into());
    window.tint = Some([10, 20, 30]);
    m.warps = vec![window];
    let text = m.to_ron_string().expect("writes");
    assert_eq!(Montage::from_ron_str(&text).expect("reads"), m);
    m.warps[0].anchor = WarpAnchor::Offset([0.0, 0.5, -0.8]);
    let text = m.to_ron_string().expect("writes");
    assert_eq!(Montage::from_ron_str(&text).expect("reads"), m);
}

#[test]
fn validation_rejects_bad_v2_fields() {
    let base = || {
        let mut m = Montage::single("m", "full_body", "dash");
        m.warps = vec![WarpWindow::new("t", 0.0, 1.0)];
        m
    };
    for fraction in [0.0, -0.5, 1.5, f32::NAN] {
        let mut m = base();
        m.warps[0].rotation_fraction = fraction;
        assert!(m.validate().is_err(), "fraction {fraction}");
    }
    let mut m = base();
    m.warps[0].anchor = WarpAnchor::Offset([f32::INFINITY, 0.0, 0.0]);
    assert!(m.validate().is_err());
    let mut m = base();
    m.warps[0].anchor = WarpAnchor::Bone(String::new());
    assert!(m.validate().is_err());

    let mut m = base();
    m.rate_curve = Some(RateCurve {
        keys: vec![(0.0, 1.0), (0.5, 2.0)],
        interpolation: RateInterpolation::Step,
    });
    assert!(m.validate().is_ok());
    for keys in [
        vec![],
        vec![(0.5, 1.0), (0.5, 2.0)],
        vec![(0.0, 0.0)],
        vec![(0.0, -1.0)],
    ] {
        m.rate_curve = Some(RateCurve {
            keys,
            interpolation: RateInterpolation::Linear,
        });
        let error = m.validate().expect_err("bad curve").to_string();
        assert!(error.contains("montage_bad_rate_curve"), "{error}");
    }
}

#[test]
fn follow_links_must_name_an_existing_segment() {
    let mut m = Montage::single("m", "full_body", "dash");
    m.sections = vec![MontageSection {
        name: "main".into(),
        start: 0.0,
        next: regia_animation::montage::SectionNext::default(),
        follow: Some(SegmentFollow {
            track: 0,
            segment: 0,
            mode: FollowMode::ShiftWithStart,
        }),
    }];
    assert!(m.validate().is_ok());
    m.notifies = vec![MontageNotify {
        name: "hit".into(),
        time: 0.1,
        duration: 0.0,
        payload: None,
        branch: false,
        follow: Some(SegmentFollow {
            track: 0,
            segment: 4,
            mode: FollowMode::KeepTime,
        }),
    }];
    assert!(m.validate().is_err());
}

// ---- warping with an anchor and a partial rotation window --------------------------------

/// Root and a hand 0.8 m in front of it (−Z).
fn skeleton() -> Skeleton {
    Skeleton::from_local_bind(
        vec!["root".into(), "hand".into()],
        vec![None, Some(0)],
        vec![translated(Vec3::ZERO), translated(Vec3::new(0.0, 0.0, -0.8))],
    )
    .expect("valid skeleton")
}

/// A 2 s dash: the root walks 1 m/s toward −Z (XZ root motion); the hand stays put.
fn dash() -> AnimationClip {
    let mut clip = AnimationClip::empty("dash", 2.0);
    clip.tracks.push(JointTrack {
        joint: 0,
        translations: TrackCurve::Animated {
            times: vec![0.0, 2.0],
            values: vec![Vec3::ZERO, Vec3::new(0.0, 0.0, -2.0)],
            exact_rotation: false,
        },
        rotations: TrackCurve::Static(Quat::IDENTITY),
        scales: TrackCurve::Static(Vec3::ONE),
    });
    clip.root_motion = Some(RootMotionTrack {
        joint: 0,
        mode: RootMotionMode::XZ,
    });
    clip
}

/// Plays a 1.6 s warp window over the dash toward `target`, applying the root motion like a
/// character controller; returns the placement after every frame.
fn run(window: WarpWindow, target: Vec3) -> Vec<RigPlacement> {
    let skeleton = skeleton();
    let mut clips = ClipLibrary::new();
    clips.insert(dash());
    let mut player = MontagePlayer::new(vec![SlotDef::full_body("full_body", "body")], clips);
    let mut montage = Montage::single("lunge", "full_body", "dash");
    montage.blend_in.time = 0.0;
    montage.warps = vec![window];
    player.play(Arc::new(montage), PlayOptions::default());
    player.set_warp_target("enemy", WarpTarget::at(target));
    let mut placement = RigPlacement::default();
    let mut trace = Vec::new();
    let mut notifies = Vec::new();
    for _ in 0..32 {
        let mut pose = skeleton.bind_pose_pose();
        let mut root = (Vec3::ZERO, Quat::IDENTITY);
        let frame = MontageFrame {
            skeleton: &skeleton,
            graph_clips: None,
            delta: 0.05,
            rig: placement,
        };
        player.evaluate(&frame, &mut pose, &mut root, &mut notifies);
        placement.position += placement.rotation * root.0;
        placement.rotation = (placement.rotation * root.1).normalize();
        trace.push(placement);
    }
    trace
}

fn hand_position(placement: &RigPlacement) -> Vec3 {
    placement.position + placement.rotation * Vec3::new(0.0, 0.0, -0.8)
}

#[test]
fn a_bone_anchor_lands_the_bone_on_the_target_not_the_origin() {
    let targets = [
        Vec3::new(1.5, 0.0, -2.5),
        Vec3::new(-1.0, 0.0, -3.0),
        Vec3::new(2.0, 0.0, -1.0),
        Vec3::new(0.5, 0.0, -4.0),
        Vec3::new(-2.5, 0.0, -2.0),
    ];
    for target in targets {
        let mut anchored = WarpWindow::new("enemy", 0.2, 1.6);
        anchored.rotation = true;
        anchored.anchor = WarpAnchor::Bone("hand".into());
        let trace = run(anchored, target);
        let last = trace.last().expect("frames");
        let miss = (hand_position(last) - target).length();
        assert!(miss < 0.01, "target {target}: hand missed by {miss}");

        // The same window anchored on the root puts the origin on the target instead.
        let mut rooted = WarpWindow::new("enemy", 0.2, 1.6);
        rooted.rotation = true;
        let trace = run(rooted, target);
        let last = trace.last().expect("frames");
        assert!((last.position - target).length() < 0.01);
        assert!((hand_position(last) - target).length() > 0.5);
    }
}

/// The dash with the hand reaching forward: joint 1 moves from 0.8 m to 1.6 m in front.
fn dash_reach() -> AnimationClip {
    let mut clip = dash();
    clip.name = "dash_reach".into();
    clip.tracks.push(JointTrack {
        joint: 1,
        translations: TrackCurve::Animated {
            times: vec![0.0, 2.0],
            values: vec![Vec3::new(0.0, 0.0, -0.8), Vec3::new(0.0, 0.0, -1.6)],
            exact_rotation: false,
        },
        rotations: TrackCurve::Static(Quat::IDENTITY),
        scales: TrackCurve::Static(Vec3::ONE),
    });
    clip
}

/// Plays `montage` toward `target` on `player` for `frames` frames of 0.05 s; the placements.
fn run_on(player: &mut MontagePlayer, montage: Montage, target: Vec3, frames: usize) -> Vec<RigPlacement> {
    let skeleton = skeleton();
    player.play(Arc::new(montage), PlayOptions::default());
    player.set_warp_target("enemy", WarpTarget::at(target));
    let mut placement = RigPlacement::default();
    let mut trace = Vec::new();
    let mut notifies = Vec::new();
    for _ in 0..frames {
        let mut pose = skeleton.bind_pose_pose();
        let mut root = (Vec3::ZERO, Quat::IDENTITY);
        let frame = MontageFrame {
            skeleton: &skeleton,
            graph_clips: None,
            delta: 0.05,
            rig: placement,
        };
        player.evaluate(&frame, &mut pose, &mut root, &mut notifies);
        placement.position += placement.rotation * root.0;
        placement.rotation = (placement.rotation * root.1).normalize();
        trace.push(placement);
    }
    trace
}

/// audit-gameplay (Medium): bone anchor offsets were cached by montage name, so a montage
/// replaced under the same name (here: one whose hand reaches further) kept the old anchor and
/// the warp aimed the origin at `target - stale_anchor`.
#[test]
fn a_montage_replaced_under_the_same_name_measures_its_anchor_again() {
    let lunge = |clip: &str| {
        let mut montage = Montage::single("lunge", "full_body", clip);
        montage.blend_in.time = 0.0;
        let mut window = WarpWindow::new("enemy", 0.2, 1.6);
        window.rotation = true;
        window.anchor = WarpAnchor::Bone("hand".into());
        montage.warps = vec![window];
        montage
    };
    let clips = || {
        let mut clips = ClipLibrary::new();
        clips.insert(dash());
        clips.insert(dash_reach());
        clips
    };
    let target = Vec3::new(1.5, 0.0, -2.5);
    let mut fresh = MontagePlayer::new(vec![SlotDef::full_body("full_body", "body")], clips());
    let expected = run_on(&mut fresh, lunge("dash_reach"), target, 32);

    let mut reused = MontagePlayer::new(vec![SlotDef::full_body("full_body", "body")], clips());
    reused.add_montage(lunge("dash"));
    // The first version plays to the end (and is measured), then the new one replaces it.
    let _ = run_on(&mut reused, lunge("dash"), target, 60);
    reused.add_montage(lunge("dash_reach"));
    let got = run_on(&mut reused, lunge("dash_reach"), target, 32);
    let (a, b) = (expected.last().expect("frames"), got.last().expect("frames"));
    assert!(
        (a.position - b.position).length() < 1e-4,
        "fresh {:?} vs reused {:?}",
        a.position,
        b.position
    );
}

#[test]
fn an_offset_anchor_lands_the_fixed_offset_point_on_the_target() {
    let target = Vec3::new(1.0, 0.0, -3.0);
    let mut window = WarpWindow::new("enemy", 0.2, 1.6);
    window.rotation = true;
    window.anchor = WarpAnchor::Offset([0.0, 0.0, -0.8]);
    let trace = run(window, target);
    let last = trace.last().expect("frames");
    assert!((hand_position(last) - target).length() < 0.01);
}

#[test]
fn an_unknown_anchor_bone_falls_back_to_the_root() {
    let target = Vec3::new(1.5, 0.0, -2.5);
    let mut window = WarpWindow::new("enemy", 0.2, 1.6);
    window.rotation = true;
    window.anchor = WarpAnchor::Bone("no_such_bone".into());
    let trace = run(window, target);
    let last = trace.last().expect("frames");
    assert!((last.position - target).length() < 0.01);
}

#[test]
fn a_half_rotation_window_finishes_turning_at_the_midpoint_and_holds() {
    let target = Vec3::new(3.0, 0.0, -2.0);
    let mut window = WarpWindow::new("enemy", 0.2, 1.6);
    window.rotation = true;
    window.rotation_fraction = 0.5;
    let trace = run(window, target);
    let yaws: Vec<f32> = trace.iter().map(|p| yaw_of(p.rotation)).collect();
    // The turn ends at 0.2 + 0.5 * 1.4 = 0.9 s, the end of frame index 17; after that the yaw holds.
    let settled = yaws[17];
    assert!(settled.abs() > 0.5, "the rig must have turned: {settled}");
    for (i, yaw) in yaws.iter().enumerate().skip(18) {
        assert!((yaw - settled).abs() < 1e-3, "frame {i}: {yaw} vs {settled}");
    }
    // Before that it is still turning.
    assert!(yaws[8].abs() < settled.abs() - 0.05, "{yaws:?}");
}

#[test]
fn path_scale_never_changes_the_authored_heading() {
    let target = Vec3::new(1.0, 0.0, -3.0);
    let mut window = WarpWindow::new("enemy", 0.2, 1.6);
    window.mode = WarpMode::PathScale;
    let trace = run(window, target);
    let mut previous = Vec3::ZERO;
    for placement in &trace {
        let step = placement.position - previous;
        previous = placement.position;
        assert!(step.x.abs() < 1e-5, "heading changed: {step}");
    }
}

/// Audit-gameplay H3 (ADR-1809): a newer montage in another slot group whose targeted window is
/// still ahead must not take the warp from an older montage inside its own window.
#[test]
fn a_newer_montage_with_a_future_window_does_not_suppress_an_active_warp() {
    let skeleton = skeleton();
    let mut clips = ClipLibrary::new();
    clips.insert(dash());
    let mut player = MontagePlayer::new(
        vec![
            SlotDef::full_body("full_body", "body"),
            SlotDef::masked("upper", "upper", &["hand"]),
        ],
        clips,
    );
    let mut lunge = Montage::single("lunge", "full_body", "dash");
    lunge.blend_in.time = 0.0;
    let mut window = WarpWindow::new("enemy", 0.2, 1.6);
    window.rotation = true;
    lunge.warps = vec![window];
    player.play(Arc::new(lunge), PlayOptions::default());
    let mut wave = Montage::single("wave", "upper", "dash");
    wave.blend_in.time = 0.0;
    wave.root_motion = false;
    wave.warps = vec![WarpWindow::new("friend", 1.7, 1.9)];
    player.play(Arc::new(wave), PlayOptions::default());
    let target = Vec3::new(1.5, 0.0, -2.5);
    player.set_warp_target("enemy", WarpTarget::at(target));
    player.set_warp_target("friend", WarpTarget::at(Vec3::new(-5.0, 0.0, 0.0)));
    assert_eq!(player.instances().len(), 2, "both montages play");
    let mut placement = RigPlacement::default();
    let mut notifies = Vec::new();
    for _ in 0..32 {
        let mut pose = skeleton.bind_pose_pose();
        let mut root = (Vec3::ZERO, Quat::IDENTITY);
        let frame = MontageFrame {
            skeleton: &skeleton,
            graph_clips: None,
            delta: 0.05,
            rig: placement,
        };
        player.evaluate(&frame, &mut pose, &mut root, &mut notifies);
        placement.position += placement.rotation * root.0;
        placement.rotation = (placement.rotation * root.1).normalize();
    }
    let miss = (placement.position - target).length();
    assert!(
        miss < 0.01,
        "the lunge must still land on its target: missed by {miss}"
    );
}

/// Audit-gameplay zai L4 / muse M (ADR-1809): a bone anchor is not served from a stale cache after
/// the clip behind a montage changes.
#[test]
fn a_replaced_clip_re_samples_the_bone_anchor() {
    let skeleton = skeleton();
    let with_hand = |z: f32| {
        let mut clip = dash();
        clip.tracks.push(JointTrack {
            joint: 1,
            translations: TrackCurve::Static(Vec3::new(0.0, 0.0, z)),
            rotations: TrackCurve::Static(Quat::IDENTITY),
            scales: TrackCurve::Static(Vec3::ONE),
        });
        let mut clips = ClipLibrary::new();
        clips.insert(clip);
        clips
    };
    let mut player = MontagePlayer::new(vec![SlotDef::full_body("full_body", "body")], with_hand(-0.8));
    let target = Vec3::new(1.5, 0.0, -2.5);
    player.set_warp_target("enemy", WarpTarget::at(target));
    let lunge = || {
        let mut montage = Montage::single("lunge", "full_body", "dash");
        montage.blend_in.time = 0.0;
        let mut window = WarpWindow::new("enemy", 0.2, 1.6);
        window.rotation = true;
        window.anchor = WarpAnchor::Bone("hand".into());
        montage.warps = vec![window];
        Arc::new(montage)
    };
    let play = |player: &mut MontagePlayer| {
        player.play(lunge(), PlayOptions::default());
        let mut placement = RigPlacement::default();
        let mut notifies = Vec::new();
        for _ in 0..32 {
            let mut pose = skeleton.bind_pose_pose();
            let mut root = (Vec3::ZERO, Quat::IDENTITY);
            let frame = MontageFrame {
                skeleton: &skeleton,
                graph_clips: None,
                delta: 0.05,
                rig: placement,
            };
            player.evaluate(&frame, &mut pose, &mut root, &mut notifies);
            placement.position += placement.rotation * root.0;
            placement.rotation = (placement.rotation * root.1).normalize();
        }
        // Let the montage end so the next play starts fresh.
        for _ in 0..40 {
            let mut pose = skeleton.bind_pose_pose();
            let mut root = (Vec3::ZERO, Quat::IDENTITY);
            let frame = MontageFrame {
                skeleton: &skeleton,
                graph_clips: None,
                delta: 0.05,
                rig: placement,
            };
            player.evaluate(&frame, &mut pose, &mut root, &mut notifies);
        }
        placement
    };
    let first = play(&mut player);
    let hand_at = |p: &RigPlacement, z: f32| p.position + p.rotation * Vec3::new(0.0, 0.0, z);
    assert!((hand_at(&first, -0.8) - target).length() < 0.01);
    // The clip's hand now sits 1.6 m out: the anchor must follow.
    player.clips = Arc::new(with_hand(-1.6));
    let second = play(&mut player);
    let miss = (hand_at(&second, -1.6) - target).length();
    assert!(miss < 0.01, "stale anchor: the hand missed by {miss}");
}

/// Audit v033b (Med): the bone-anchor cache kept one entry per distinct montage ever played, each
/// holding an `Arc` of the montage and of the clip library of that frame. A finished montage that
/// is not in the library, and a replaced clip library, are released at the end of the frame.
#[test]
fn finished_montages_and_replaced_clip_libraries_leave_the_anchor_cache() {
    let skeleton = skeleton();
    let mut clips = ClipLibrary::new();
    clips.insert(dash());
    let mut player = MontagePlayer::new(vec![SlotDef::full_body("full_body", "body")], clips);
    let old_clips = Arc::clone(&player.clips);
    let mut played = Vec::new();
    for n in 0..3 {
        let mut montage = Montage::single(format!("lunge_{n}"), "full_body", "dash");
        montage.blend_in.time = 0.0;
        let mut window = WarpWindow::new("enemy", 0.2, 1.6);
        window.anchor = WarpAnchor::Bone("hand".into());
        montage.warps = vec![window];
        let montage = Arc::new(montage);
        player.play(Arc::clone(&montage), PlayOptions::default());
        player.set_warp_target("enemy", WarpTarget::at(Vec3::new(1.0, 0.0, -2.0)));
        let mut notifies = Vec::new();
        for _ in 0..80 {
            let mut pose = skeleton.bind_pose_pose();
            let mut root = (Vec3::ZERO, Quat::IDENTITY);
            let frame = MontageFrame {
                skeleton: &skeleton,
                graph_clips: None,
                delta: 0.05,
                rig: RigPlacement::default(),
            };
            player.evaluate(&frame, &mut pose, &mut root, &mut notifies);
        }
        played.push(montage);
    }
    for montage in &played {
        assert_eq!(
            Arc::strong_count(montage),
            1,
            "{}: a finished montage outside the library is not pinned",
            montage.name
        );
    }
    player.clips = Arc::new(ClipLibrary::new());
    let mut notifies = Vec::new();
    let mut pose = skeleton.bind_pose_pose();
    let mut root = (Vec3::ZERO, Quat::IDENTITY);
    player.evaluate(
        &MontageFrame {
            skeleton: &skeleton,
            graph_clips: None,
            delta: 0.05,
            rig: RigPlacement::default(),
        },
        &mut pose,
        &mut root,
        &mut notifies,
    );
    assert_eq!(
        Arc::strong_count(&old_clips),
        1,
        "the replaced clip library is released"
    );
}
