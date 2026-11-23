// SPDX-License-Identifier: MPL-2.0
//! Spec 34 conformance (`docs/specs/34-montage-extensions.md`): acceptance items of §§ 3-4 that
//! the montage test files do not prove as written: the bone anchor over 10 random targets and 2 yaw
//! settings, an unset-target window being inactive, a v1 file playing bit-identically to its
//! explicit v2 form, `PathScale` combined with `rotation`, and two rigs of one skeleton resolving a
//! montage identically from one slot table. § 5 adds a scripted warp over a clip with no root
//! motion spreading linearly, landing on the target and warning once per warp.
//!
//! Every input is fixed (seeded targets, frame times, clips); no wall clock.

use std::sync::Arc;

use regia_animation::clip::{AnimationClip, JointTrack, RootMotionMode, RootMotionTrack, TrackCurve};
use regia_animation::montage::WarpWindow;
use regia_animation::montage_player::{MontageFrame, RigPlacement};
use regia_animation::pose::translated;
use regia_animation::skeleton_slots::SkeletonSlots;
use regia_animation::warp::{WarpAnchor, WarpMode, yaw_of};
use regia_animation::{
    ClipLibrary, Montage, MontagePlayer, PlayOptions, Pose, Skeleton, SlotDef, WarpTarget,
};
use regia_core::math::{Quat, Rng, Vec3};

/// Root, a spine and a hand 0.8 m in front of the root (−Z).
fn skeleton() -> Skeleton {
    Skeleton::from_local_bind(
        vec!["root".into(), "spine".into(), "hand".into()],
        vec![None, Some(0), Some(0)],
        vec![
            translated(Vec3::ZERO),
            translated(Vec3::new(0.0, 1.0, 0.0)),
            translated(Vec3::new(0.0, 0.0, -0.8)),
        ],
    )
    .expect("valid skeleton")
}

/// A 2 s dash: the root walks 1 m/s toward −Z (XZ root motion); the spine sways.
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
    clip.tracks.push(JointTrack {
        joint: 1,
        translations: TrackCurve::Animated {
            times: vec![0.0, 1.0, 2.0],
            values: vec![
                Vec3::new(0.0, 1.0, 0.0),
                Vec3::new(0.2, 1.1, 0.0),
                Vec3::new(0.0, 1.0, 0.0),
            ],
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

fn clips() -> ClipLibrary {
    let mut clips = ClipLibrary::new();
    clips.insert(dash());
    clips
}

/// Plays `montage` on a fresh full-body player starting at `start`, with the `enemy` target at
/// `target` (or unset), applying root motion like a character controller; the placement after
/// every 0.05 s frame.
fn run(montage: Montage, start: RigPlacement, target: Option<Vec3>) -> Vec<RigPlacement> {
    let skeleton = skeleton();
    let mut player = MontagePlayer::new(vec![SlotDef::full_body("full_body", "body")], clips());
    player.play(Arc::new(montage), PlayOptions::default());
    if let Some(target) = target {
        player.set_warp_target("enemy", WarpTarget::at(target));
    }
    let mut placement = start;
    let mut trace = Vec::new();
    let mut notifies = Vec::new();
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
        placement.position += placement.rotation * root.0;
        placement.rotation = (placement.rotation * root.1).normalize();
        trace.push(placement);
    }
    trace
}

fn lunge(window: Option<WarpWindow>) -> Montage {
    let mut montage = Montage::single("lunge", "full_body", "dash");
    montage.blend_in.time = 0.0;
    montage.warps = window.into_iter().collect();
    montage
}

fn hand_position(placement: &RigPlacement) -> Vec3 {
    placement.position + placement.rotation * Vec3::new(0.0, 0.0, -0.8)
}

fn at_yaw(yaw: f32) -> RigPlacement {
    RigPlacement {
        rotation: Quat::from_rotation_y(yaw),
        ..RigPlacement::default()
    }
}

/// The placement at the window end (1.6 s = frame index 31).
const WINDOW_END_FRAME: usize = 31;

// § 4 acceptance: "a `Bone` anchor 0.8 m ahead of the origin makes the bone, not the origin, land
// within 1 cm of the target at `end` for 10 random targets and 2 yaw settings". The targets are
// seeded and lie ahead of the rig (2-4 m, within 45 degrees of its facing: a hand reaching along
// the direction of travel, the well-conditioned case of § 4a bound 4).
#[test]
fn a_bone_anchor_lands_within_a_centimetre_for_ten_random_targets_and_two_yaws() {
    let mut rng = Rng::new(0x5_4a11);
    let targets: Vec<(f32, f32)> = (0..10)
        .map(|_| {
            let distance = 2.0 + 2.0 * rng.f32();
            let bearing = (rng.f32() - 0.5) * std::f32::consts::FRAC_PI_2;
            (distance, bearing)
        })
        .collect();
    for start_yaw in [0.0f32, 1.1] {
        let start = at_yaw(start_yaw);
        for &(distance, bearing) in &targets {
            // Rig forward is −Z, turned by the start yaw and the bearing.
            let target = Quat::from_rotation_y(start_yaw + bearing) * Vec3::new(0.0, 0.0, -distance);
            let mut window = WarpWindow::new("enemy", 0.2, 1.6);
            window.rotation = true;
            window.anchor = WarpAnchor::Bone("hand".into());
            let trace = run(lunge(Some(window)), start, Some(target));
            let end = &trace[WINDOW_END_FRAME];
            let miss = (hand_position(end) - target).length();
            assert!(
                miss < 0.01,
                "yaw {start_yaw}, target {target}: the hand missed by {miss}"
            );
            assert!(
                (end.position - target).length() > 0.5,
                "the origin, not the hand, stops short of the target"
            );
        }
    }
}

// § 4 rule 4: "a window whose named target is not set is inactive (no correction, no error)".
#[test]
fn a_window_whose_target_is_unset_is_inactive() {
    let mut window = WarpWindow::new("enemy", 0.2, 1.6);
    window.rotation = true;
    window.anchor = WarpAnchor::Bone("hand".into());
    let unwarped = run(lunge(None), RigPlacement::default(), None);
    let unset = run(lunge(Some(window.clone())), RigPlacement::default(), None);
    assert_eq!(
        unwarped, unset,
        "an unset target leaves the authored motion bit for bit"
    );
    // The same window with the target set does warp (the comparison above is not vacuous).
    let set = run(
        lunge(Some(window)),
        RigPlacement::default(),
        Some(Vec3::new(2.0, 0.0, -1.0)),
    );
    assert_ne!(unwarped, set);
}

const V1: &str = r#"(
    format_version: 1,
    name: "lunge",
    tracks: [(slot: "full_body", segments: [(clip: "dash")])],
    blend_in: (time: 0.0),
    warps: [(target: "enemy", start: 0.2, end: 1.6, rotation: true)],
)"#;

const V2_EXPLICIT: &str = r#"(
    format_version: 2,
    name: "lunge",
    tracks: [(slot: "full_body", segments: [(clip: "dash")])],
    blend_in: (time: 0.0),
    warps: [(target: "enemy", start: 0.2, end: 1.6, rotation: true,
             mode: Similarity, rotation_kind: Auto, rotation_fraction: 1.0, anchor: Root)],
)"#;

// § 4 migration and acceptance: "a v1 file migrates and plays bit-identically" — every v2 field
// defaults to the v1 behavior, so the v1 file and the same montage written as v2 with the v1
// behavior spelled out give one root-motion trace, bit for bit.
#[test]
fn a_v1_file_plays_bit_identically_to_its_explicit_v2_form() {
    let v1 = Montage::from_ron_str(V1).expect("v1 loads");
    let v2 = Montage::from_ron_str(V2_EXPLICIT).expect("v2 loads");
    assert_eq!(v1, v2, "the migration fills exactly the v1 defaults");
    let target = Some(Vec3::new(1.5, 0.0, -2.5));
    let a = run(v1, RigPlacement::default(), target);
    let b = run(v2, RigPlacement::default(), target);
    assert_eq!(a, b);
    // The trace really is warped (the comparison is not of two unwarped runs).
    assert!((a[WINDOW_END_FRAME].position - Vec3::new(1.5, 0.0, -2.5)).length() < 0.01);
}

// § 4 rule 6: `PathScale` combined with `rotation: true` is allowed: the path is scaled and the
// facing turns. The rig ends the window facing the target, and the distance covered inside the
// window is the scaled remaining offset (target distance 3.6 m over 1.4 m of authored motion).
#[test]
fn path_scale_with_rotation_scales_the_path_and_turns_the_rig() {
    let target = Vec3::new(0.0, 0.0, -3.8);
    let mut window = WarpWindow::new("enemy", 0.2, 1.6);
    window.mode = WarpMode::PathScale;
    window.rotation = true;
    let trace = run(lunge(Some(window.clone())), RigPlacement::default(), Some(target));
    let end = &trace[WINDOW_END_FRAME];
    // The target is straight ahead: no turn is needed and PathScale lands on it.
    assert!((end.position - target).length() < 0.01, "{:?}", end.position);
    assert!(yaw_of(end.rotation).abs() < 1e-4);

    // A target off to the side: the facing turns toward it while the scaled path keeps going.
    let side = Vec3::new(2.0, 0.0, -3.0);
    let trace = run(lunge(Some(window)), RigPlacement::default(), Some(side));
    let end = &trace[WINDOW_END_FRAME];
    let turned = yaw_of(end.rotation);
    let wanted = yaw_of(Quat::from_rotation_arc(
        Vec3::NEG_Z,
        (side - trace[3].position).normalize(),
    ));
    assert!(turned.abs() > 0.3, "the rig turned: {turned}");
    assert!(
        turned.signum() == wanted.signum(),
        "toward the target: {turned} vs {wanted}"
    );
    for placement in &trace {
        assert!(placement.position.is_finite() && placement.rotation.is_finite());
    }
}

// § 3 acceptance: "two rigs of one skeleton resolve one montage identically" — players built from
// one slot table (here with a masked upper-body slot) give the same pose and the same root motion.
#[test]
fn two_rigs_of_one_skeleton_resolve_one_montage_identically() {
    let table = SkeletonSlots::from_ron_str(
        r#"(format_version: 1, slots: [
            (name: "upper_body", group: "upper", mask: Some((joints: ["spine"], include_children: true))),
            (name: "full", group: "default", mask: None),
        ])"#,
    )
    .expect("the table loads");
    let skeleton = skeleton();
    table
        .validate_against(&skeleton)
        .expect("every mask joint exists");
    let mut montage = Montage::single("wave", "upper_body", "dash");
    montage.blend_in.time = 0.1;
    let montage = Arc::new(montage);
    let mut rigs: Vec<MontagePlayer> = (0..2)
        .map(|_| MontagePlayer::new(table.slot_defs(), clips()))
        .collect();
    for rig in &mut rigs {
        rig.play(Arc::clone(&montage), PlayOptions::default());
    }
    let mut notifies = Vec::new();
    for frame_index in 0..30 {
        let mut out: Vec<(Pose, (Vec3, Quat))> = Vec::new();
        for rig in &mut rigs {
            let mut pose = skeleton.bind_pose_pose();
            let mut root = (Vec3::ZERO, Quat::IDENTITY);
            let frame = MontageFrame {
                skeleton: &skeleton,
                graph_clips: None,
                delta: 0.05,
                rig: RigPlacement::default(),
            };
            rig.evaluate(&frame, &mut pose, &mut root, &mut notifies);
            out.push((pose, root));
        }
        assert_eq!(out[0], out[1], "frame {frame_index}");
        if frame_index == 10 {
            // The slot's mask applies: the spine moves, the hand (outside the mask) keeps its bind pose.
            assert_ne!(out[0].0.translations[1], Vec3::new(0.0, 1.0, 0.0));
            assert_eq!(out[0].0.translations[2], Vec3::new(0.0, 0.0, -0.8));
        }
    }
}

// § 6: everything but the overrides is inherited and read-only in a variant: "the file cannot
// name other fields". A variant that tries to change the parent's sections, or an override that
// names a field overrides do not have, is refused instead of silently ignored.
#[test]
fn a_variant_file_cannot_name_other_fields() {
    use regia_animation::montage_variant::MontageVariant;
    let good = r#"(format_version: 1, name: "wave_short", parent: "wave",
        overrides: [(track: 0, segment: 0, clip: "WaveShort", rescale: true)])"#;
    assert!(MontageVariant::from_ron_str(good).is_ok());
    let sections = r#"(format_version: 1, name: "wave_short", parent: "wave",
        overrides: [(track: 0, segment: 0, clip: "WaveShort")], sections: [])"#;
    assert!(
        MontageVariant::from_ron_str(sections).is_err(),
        "a variant may not name the parent's sections"
    );
    let override_rate = r#"(format_version: 1, name: "wave_short", parent: "wave",
        overrides: [(track: 0, segment: 0, clip: "WaveShort", rate: 2.0)])"#;
    assert!(
        MontageVariant::from_ron_str(override_rate).is_err(),
        "an override may not name a field it does not have"
    );
}

/// What a test-local tracing subscriber saw: span names and event messages.
#[derive(Default)]
struct Seen {
    spans: Vec<String>,
    messages: Vec<String>,
}

struct Capture(std::sync::Arc<std::sync::Mutex<Seen>>);

struct MessageText(String);

impl tracing::field::Visit for MessageText {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{value:?}");
        }
    }
}

impl tracing::Subscriber for Capture {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, span: &tracing::span::Attributes<'_>) -> tracing::Id {
        if let Ok(mut seen) = self.0.lock() {
            seen.spans.push(span.metadata().name().to_string());
        }
        tracing::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::Id, _: &tracing::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let mut text = MessageText(String::new());
        event.record(&mut text);
        if let Ok(mut seen) = self.0.lock() {
            seen.messages.push(text.0);
        }
    }
    fn enter(&self, _: &tracing::Id) {}
    fn exit(&self, _: &tracing::Id) {}
}

// § 5: a scripted warp over a clip with no root motion falls back to the linear spread (it still
// lands on the target) and logs `warp_no_root_motion` once per scripted warp; adding the id again
// after the first warp has finished and been removed warns once more (a fresh warp, not replacing
// a live one). § 10: the corrector runs inside the `scripted_warps` span.
#[test]
fn a_scripted_warp_without_root_motion_spreads_linearly_and_warns_once() {
    use regia_animation::{ScriptedWarp, ScriptedWarps};
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Seen::default()));
    let target = Vec3::new(1.0, 0.0, -2.0);
    let no_bone: &dyn Fn(&str) -> Option<Vec3> = &|_| None;
    let end = tracing::subscriber::with_default(Capture(std::sync::Arc::clone(&seen)), || {
        let mut warps = ScriptedWarps::new();
        warps.set_target("enemy", WarpTarget::at(target));
        warps
            .add(ScriptedWarp::new("dash", "enemy", 1.0))
            .expect("valid warp");
        let mut position = Vec3::ZERO;
        for _ in 0..20 {
            let rig = RigPlacement {
                position,
                ..RigPlacement::default()
            };
            let (t, _) = warps.apply(0.05, rig, (Vec3::ZERO, Quat::IDENTITY), no_bone);
            position += t;
        }
        // The same id again: a fresh warp, one more warning.
        warps
            .add(ScriptedWarp::new("dash", "enemy", 1.0))
            .expect("valid warp");
        for _ in 0..5 {
            warps.apply(
                0.05,
                RigPlacement::default(),
                (Vec3::ZERO, Quat::IDENTITY),
                no_bone,
            );
        }
        position
    });
    assert!((end - target).length() < 0.01, "the linear spread lands: {end}");
    let seen = seen.lock().expect("not poisoned");
    let warnings = seen
        .messages
        .iter()
        .filter(|m| m.contains("warp_no_root_motion"))
        .count();
    assert_eq!(warnings, 2, "{:?}", seen.messages);
    assert!(
        seen.spans.iter().any(|s| s == "scripted_warps"),
        "{:?}",
        seen.spans
    );
}
