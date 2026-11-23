// SPDX-License-Identifier: MPL-2.0
//! Warp windows split a step's root motion at their edges (ADR-2604,
//! `docs/specs/34-montage-extensions.md` § 4a): when a frame straddles a window's start or
//! end, only the part of the frame inside the window is warped and the authored motion
//! outside it passes through untouched.
//!
//! The dash is 1 m/s toward −Z for 2 s (XZ root motion); every input is fixed (frame times,
//! targets, clips); no wall clock.

use std::sync::Arc;

use regia_animation::clip::{AnimationClip, JointTrack, RootMotionMode, RootMotionTrack, TrackCurve};
use regia_animation::montage::WarpWindow;
use regia_animation::montage_player::{MontageFrame, RigPlacement};
use regia_animation::pose::translated;
use regia_animation::warp::{WarpInput, WarpMode, warp_frame, yaw_of, yaw_toward};
use regia_animation::{ClipLibrary, Montage, MontagePlayer, PlayOptions, Skeleton, SlotDef, WarpTarget};
use regia_core::math::{Quat, Vec3};

/// Unit roundoff of `f32`.
const U: f32 = f32::EPSILON / 2.0;

fn skeleton() -> Skeleton {
    Skeleton::from_local_bind(vec!["root".into()], vec![None], vec![translated(Vec3::ZERO)])
        .expect("valid skeleton")
}

/// The 2 s dash, 1 m/s toward −Z.
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

/// Plays the dash with one warp `window` toward `target` in frames of `dts` (cycled until
/// `total` seconds have passed), applying root motion like a character controller; returns
/// the placement after every frame together with the montage time it reached.
fn run(window: WarpWindow, target: Vec3, dts: &[f32], total: f32) -> Vec<(f32, RigPlacement)> {
    let skeleton = skeleton();
    let mut clips = ClipLibrary::new();
    clips.insert(dash());
    let mut player = MontagePlayer::new(vec![SlotDef::full_body("full_body", "body")], clips);
    let mut montage = Montage::single("lunge", "full_body", "dash");
    montage.blend_in.time = 0.0;
    montage.blend_out.time = 0.0;
    montage.warps = vec![window];
    player.play(Arc::new(montage), PlayOptions::default());
    player.set_warp_target("enemy", WarpTarget::at(target));
    let mut placement = RigPlacement::default();
    let mut trace = Vec::new();
    let mut notifies = Vec::new();
    let mut time = 0.0f32;
    let mut k = 0;
    while time < total - 1e-6 {
        let dt = dts[k % dts.len()].min(total - time);
        k += 1;
        time += dt;
        let mut pose = skeleton.bind_pose_pose();
        let mut root = (Vec3::ZERO, Quat::IDENTITY);
        let frame = MontageFrame {
            skeleton: &skeleton,
            graph_clips: None,
            delta: dt,
            rig: placement,
        };
        player.evaluate(&frame, &mut pose, &mut root, &mut notifies);
        placement.position += placement.rotation * root.0;
        placement.rotation = (placement.rotation * root.1).normalize();
        trace.push((time, placement));
    }
    trace
}

/// Authored position of the dash at montage time `t` (from the origin, no warp).
fn authored(t: f32) -> Vec3 {
    Vec3::new(0.0, 0.0, -t.clamp(0.0, 2.0))
}

/// What a frame `[from, to]` wholly inside a translation-only window `[start, end]` should
/// do from `at`: the reference warp of the inside piece alone.
fn reference_piece(at: Vec3, from: f32, to: f32, end: f32, target: Vec3) -> Vec3 {
    let (t, _) = warp_frame(&WarpInput {
        frame: (authored(to) - authored(from), Quat::IDENTITY),
        rest: (authored(end) - authored(to), Quat::IDENTITY),
        to_target: target - at,
        yaw_to_target: None,
        frame_time: to - from,
        rest_time: end - to,
        horizontal: true,
        vertical: false,
        mode: WarpMode::Similarity,
        rotation_frame_time: to - from,
        rotation_rest_time: end - to,
    });
    at + t
}

/// A frame straddling the window start: the 0.05 s before the start is authored, and only the
/// 0.1 s inside is warped, from where the rig stands at the start. The old code warped the
/// whole frame (the outside 5 cm were bent toward the target too).
#[test]
fn a_step_straddling_the_start_warps_only_the_inside_part() {
    let target = Vec3::new(1.5, 0.0, -2.5);
    let trace = run(WarpWindow::new("enemy", 0.2, 1.6), target, &[0.15], 0.3);
    // Frame 0 ends at 0.15 s, before the window: authored.
    assert!(
        (trace[0].1.position - authored(0.15)).length() < 1e-6,
        "{:?}",
        trace[0]
    );
    // Frame 1 runs 0.15 → 0.30 s across the start at 0.2 s.
    let expected = reference_piece(authored(0.2), 0.2, 0.3, 1.6, target);
    let got = trace[1].1.position;
    assert!((got - expected).length() < 1e-5, "{got} vs {expected}");
}

/// A frame straddling the window end: the rig is on the target at the end, then the authored
/// motion after the end follows. The old code mapped the whole frame onto the target, so the
/// 0.05 s after the end was lost.
#[test]
fn a_step_straddling_the_end_lands_on_the_target_then_moves_as_authored() {
    let target = Vec3::new(1.5, 0.0, -2.5);
    // Frames end at 0.15, 0.30, ..., 1.50, 1.65 (straddles 1.6), ..., 1.95, 2.0.
    let trace = run(WarpWindow::new("enemy", 0.2, 1.6), target, &[0.15], 2.0);
    let straddle = trace
        .iter()
        .find(|(t, _)| *t > 1.6)
        .map(|(_, p)| p.position)
        .expect("a frame past the end");
    let expected = target + Vec3::new(0.0, 0.0, -0.05);
    assert!((straddle - expected).length() < 1e-4, "{straddle} vs {expected}");
    let last = trace.last().expect("frames").1.position;
    let expected = target + Vec3::new(0.0, 0.0, -0.4);
    assert!((last - expected).length() < 1e-4, "{last} vs {expected}");
}

/// A window wholly inside one frame (both edges straddled): before, inside and after are
/// split; the inside piece lands on the target and the authored remainder follows.
#[test]
fn a_window_inside_one_step_is_split_at_both_edges() {
    let target = Vec3::new(1.0, 0.0, -1.0);
    let trace = run(WarpWindow::new("enemy", 0.2, 0.3), target, &[0.5], 0.5);
    let got = trace[0].1.position;
    // 0.2 m authored, then the window ends on the target, then 0.2 m authored.
    let expected = target + Vec3::new(0.0, 0.0, -0.2);
    assert!((got - expected).length() < 1e-5, "{got} vs {expected}");
    // One frame over the whole montage, its window strictly inside.
    let trace = run(WarpWindow::new("enemy", 0.2, 1.6), target, &[2.0], 2.0);
    let got = trace[0].1.position;
    let expected = target + Vec3::new(0.0, 0.0, -0.4);
    assert!((got - expected).length() < 1e-5, "{got} vs {expected}");
}

/// A frame that straddles both the start and the end of a rotating window: the rig faces the
/// target at the end, and the authored motion after the end goes along the new facing.
#[test]
fn a_rotating_window_inside_one_step_turns_the_motion_after_it() {
    let target = Vec3::new(2.0, 0.0, -1.0);
    let mut window = WarpWindow::new("enemy", 0.2, 0.3);
    window.rotation = true;
    let trace = run(window, target, &[0.5], 0.5);
    let placement = trace[0].1;
    // The facing at the end of the window looks from where the window started toward the
    // target (the inside piece lands on it, so its aim is that direction).
    let facing = yaw_toward(target - authored(0.2)).expect("a direction");
    assert!(
        (yaw_of(placement.rotation) - facing).abs() < 1e-4,
        "{placement:?}"
    );
    let expected = target + Quat::from_rotation_y(facing) * Vec3::new(0.0, 0.0, -0.2);
    assert!(
        (placement.position - expected).length() < 1e-4,
        "{} vs {expected}",
        placement.position
    );
}

/// A zero-length window is a load error, and a host that builds one anyway gets the authored
/// motion: it never warps (and never divides by its zero length).
#[test]
fn a_zero_length_window_is_refused_and_never_warps() {
    let mut montage = Montage::single("lunge", "full_body", "dash");
    montage.warps = vec![WarpWindow::new("enemy", 0.5, 0.5)];
    assert!(montage.validate().is_err());
    let target = Vec3::new(3.0, 0.0, 0.0);
    let cadences: [&[f32]; 4] = [&[0.15], &[0.5], &[1.0], &[2.0]];
    for dts in cadences {
        let trace = run(WarpWindow::new("enemy", 0.5, 0.5), target, dts, 2.0);
        for (t, placement) in &trace {
            assert!(
                (placement.position - authored(*t)).length() < 1e-5,
                "dt {dts:?} at {t}: {placement:?}"
            );
            assert_eq!(placement.rotation, Quat::IDENTITY);
        }
    }
}

/// The derived bound of spec 34 § 4a: for a translation window and a fixed target, the rig at
/// the montage end is the target plus the authored motion after the window, within
/// `(2N + 8)·u·S` (N frames, u the unit roundoff, S the path length scale), whatever the frame
/// times. Before the split the error was the authored motion of the part of the last
/// straddling frame past the end, up to `v·dt`.
#[test]
fn total_displacement_matches_the_target_within_the_derived_bound() {
    let targets = [
        Vec3::new(1.5, 0.0, -2.5),
        Vec3::new(-3.0, 0.0, -1.0),
        Vec3::new(0.2, 0.0, 4.0),
        Vec3::new(6.0, 0.0, -6.0),
    ];
    let cadences: [&[f32]; 7] = [
        &[1.0 / 30.0],
        &[1.0 / 60.0],
        &[1.0 / 144.0],
        &[0.07],
        &[0.15],
        &[0.33],
        &[0.011, 0.05, 0.13, 0.021],
    ];
    let mut worst = 0.0f32;
    for target in targets {
        for dts in cadences {
            let trace = run(WarpWindow::new("enemy", 0.2, 1.6), target, dts, 2.0);
            let frames = trace.len() as f32;
            let last = trace.last().expect("frames").1.position;
            let expected = target + (authored(2.0) - authored(1.6));
            let scale = authored(0.2).length() + (target - authored(0.2)).length() + 0.4;
            let bound = (2.0 * frames + 8.0) * U * scale;
            let error = (last - expected).length();
            worst = worst.max(error / bound);
            assert!(
                error <= bound,
                "target {target}, dt {dts:?}: off by {error} m, bound {bound} m ({frames} frames)"
            );
        }
    }
    println!("worst error / bound = {worst}");
}

/// Rule 2 of spec 34 § 4 as re-derived in § 4a: when the facing does not depend on the aim
/// (here `MatchTarget` with a target facing), the two-pass anchor solve is exact, so a 2 m
/// anchor lands on the target to rounding (well under the 1e-3 m of rule 2) for every frame
/// cadence, straddling frames included.
#[test]
fn a_two_metre_anchor_with_a_fixed_facing_lands_within_the_rule_2_bound() {
    use regia_animation::warp::{RotationKind, WarpAnchor};
    let anchor = Vec3::new(0.0, 0.0, -2.0);
    for (target, facing) in [
        (Vec3::new(1.5, 0.0, -4.5), 0.4f32),
        (Vec3::new(-3.0, 0.0, -2.0), 1.3),
        (Vec3::new(2.0, 0.0, 3.0), -2.5),
    ] {
        let cadences: [&[f32]; 4] = [&[0.05], &[0.15], &[1.0 / 60.0], &[0.33]];
        for dts in cadences {
            let mut window = WarpWindow::new("enemy", 0.2, 1.6);
            window.rotation = true;
            window.rotation_kind = RotationKind::MatchTarget;
            window.anchor = WarpAnchor::Offset(anchor.to_array());
            let skeleton = skeleton();
            let mut clips = ClipLibrary::new();
            clips.insert(dash());
            let mut player = MontagePlayer::new(vec![SlotDef::full_body("full_body", "body")], clips);
            let mut montage = Montage::single("lunge", "full_body", "dash");
            montage.blend_in.time = 0.0;
            montage.blend_out.time = 0.0;
            montage.warps = vec![window];
            player.play(Arc::new(montage), PlayOptions::default());
            player.set_warp_target(
                "enemy",
                WarpTarget {
                    position: target,
                    facing: Some(Quat::from_rotation_y(facing)),
                },
            );
            let mut placement = RigPlacement::default();
            let mut notifies = Vec::new();
            let (mut time, mut k) = (0.0f32, 0);
            while time < 2.0 - 1e-6 {
                let dt = dts[k % dts.len()].min(2.0 - time);
                k += 1;
                time += dt;
                let mut pose = skeleton.bind_pose_pose();
                let mut root = (Vec3::ZERO, Quat::IDENTITY);
                let frame = MontageFrame {
                    skeleton: &skeleton,
                    graph_clips: None,
                    delta: dt,
                    rig: placement,
                };
                player.evaluate(&frame, &mut pose, &mut root, &mut notifies);
                placement.position += placement.rotation * root.0;
                placement.rotation = (placement.rotation * root.1).normalize();
            }
            // After the window the rig no longer turns; step back along the authored 0.4 m.
            assert!(
                (yaw_of(placement.rotation) - facing).abs() < 1e-4,
                "{placement:?}"
            );
            let at_end = placement.position - placement.rotation * Vec3::new(0.0, 0.0, -0.4);
            let landed = at_end + placement.rotation * anchor;
            let miss = (landed - target).length();
            assert!(
                miss < 1e-3,
                "target {target}, facing {facing}, dt {dts:?}: anchor missed by {miss} m"
            );
        }
    }
}
