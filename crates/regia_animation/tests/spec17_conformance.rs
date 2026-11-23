// SPDX-License-Identifier: MPL-2.0
//! Spec 17 conformance (lane sprint-spec-06-16-audio, ADR-5614): tests for behavior the code
//! already had but no test pinned. Montage event history, `blend_out_lead`, reverse playback
//! with warp windows, the montage clip-notify weight gate, `SyncRole::NeverLead`, blend spaces
//! reading a non-finite input as 0, and the `EyeLimits` defaults.
//!
//! Every input is fixed (frame times, weights); no wall clock.

use std::sync::Arc;

use regia_animation::blendspace::{BlendSample1D, BlendSample2D, BlendSpace1D, BlendSpace2D};
use regia_animation::clip::{AnimationClip, JointTrack, Notify, RootMotionMode, RootMotionTrack, TrackCurve};
use regia_animation::gaze::EyeLimits;
use regia_animation::montage::{MontageNotify, WarpWindow};
use regia_animation::montage_player::{EVENT_HISTORY, MontageFrame, MontageRecord, RigPlacement};
use regia_animation::pose::{Pose, translated};
use regia_animation::state_machine::TransitionCurve;
use regia_animation::sync::{SyncGroupDef, SyncGroups, SyncMember, SyncRole};
use regia_animation::{
    ClipLibrary, Montage, MontageEventKind, MontagePhase, MontagePlayer, PlayOptions, Skeleton, SlotDef,
    SyncSetup, WarpTarget,
};
use regia_core::math::{Quat, Vec3};

fn skeleton() -> Skeleton {
    Skeleton::from_local_bind(
        vec!["root".into(), "arm".into()],
        vec![None, Some(0)],
        vec![translated(Vec3::ZERO), translated(Vec3::Y)],
    )
    .expect("valid skeleton")
}

/// A clip whose root walks 1 m/s toward −Z (XZ root motion).
fn walk(name: &str, duration: f32) -> AnimationClip {
    let mut clip = AnimationClip::empty(name, duration);
    clip.tracks.push(JointTrack {
        joint: 0,
        translations: TrackCurve::Animated {
            times: vec![0.0, duration],
            values: vec![Vec3::ZERO, Vec3::new(0.0, 0.0, -duration)],
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

fn library() -> ClipLibrary {
    let mut clips = ClipLibrary::new();
    clips.insert(walk("strike", 2.0));
    let mut tagged = walk("tagged", 1.0);
    for (time, name) in [(0.2, "early"), (0.8, "late")] {
        tagged.notifies.push(Notify {
            time,
            name: name.into(),
            payload: None,
        });
    }
    clips.insert(tagged);
    clips
}

struct Rig {
    skeleton: Skeleton,
    player: MontagePlayer,
    pose: Pose,
    root: (Vec3, Quat),
    events: Vec<MontageRecord>,
    notifies: Vec<Notify>,
}

impl Rig {
    fn new() -> Self {
        let skeleton = skeleton();
        let pose = skeleton.bind_pose_pose();
        Rig {
            skeleton,
            player: MontagePlayer::new(vec![SlotDef::full_body("full_body", "body")], library()),
            pose,
            root: (Vec3::ZERO, Quat::IDENTITY),
            events: Vec::new(),
            notifies: Vec::new(),
        }
    }

    /// One frame over a graph that holds the bind pose; returns this frame's events.
    fn step(&mut self, dt: f32) -> Vec<MontageRecord> {
        self.pose = self.skeleton.bind_pose_pose();
        self.root = (Vec3::ZERO, Quat::IDENTITY);
        let frame = MontageFrame {
            skeleton: &self.skeleton,
            graph_clips: None,
            delta: dt,
            rig: RigPlacement::default(),
        };
        self.player
            .evaluate(&frame, &mut self.pose, &mut self.root, &mut self.notifies);
        let events = self.player.take_events();
        self.events.extend(events.iter().cloned());
        events
    }
}

fn immediate(name: &str, clip: &str) -> Montage {
    let mut m = Montage::single(name, "full_body", clip);
    m.blend_in.time = 0.0;
    m
}

/// Spec 17 § 4b: "The player keeps the last 64 events for agents."
#[test]
fn the_player_keeps_the_last_64_events_oldest_first() {
    assert_eq!(EVENT_HISTORY, 64);
    let mut rig = Rig::new();
    let mut m = immediate("many", "strike");
    m.blend_out.time = 0.0;
    for i in 0..80 {
        m.notifies.push(MontageNotify {
            follow: None,
            name: format!("n{i:02}"),
            time: 0.01 + i as f32 * 0.02,
            duration: 0.0,
            payload: None,
            branch: false,
        });
    }
    rig.player.play(Arc::new(m), PlayOptions::default());
    for _ in 0..50 {
        rig.step(0.05);
    }
    assert!(rig.events.len() > EVENT_HISTORY, "{} events", rig.events.len());
    let history: Vec<&MontageRecord> = rig.player.history().collect();
    assert_eq!(history.len(), EVENT_HISTORY);
    let tail = &rig.events[rig.events.len() - EVENT_HISTORY..];
    for (kept, expected) in history.iter().zip(tail) {
        assert_eq!(kept.kind, expected.kind);
    }
}

/// The montage time at which a montage of `strike` (2 s, blend-out 0.2 s) first reports
/// `BlendingOut`, stepping 20 ms frames.
fn blend_out_start(lead: Option<f32>) -> f32 {
    let mut rig = Rig::new();
    let mut m = immediate("lead", "strike");
    m.blend_out.time = 0.2;
    m.blend_out_lead = lead;
    let id = rig.player.play(Arc::new(m), PlayOptions::default());
    for _ in 0..100 {
        let events = rig.step(0.02);
        if events
            .iter()
            .any(|e| matches!(e.kind, MontageEventKind::BlendingOut { interrupted: false }))
        {
            let status = rig.player.status(id).expect("still playing while it blends out");
            assert_eq!(status.phase, MontagePhase::BlendingOut);
            return status.time;
        }
    }
    panic!("no blend-out within 2 s for lead {lead:?}");
}

/// Spec 17 § 4b: `blend_out_lead` is the seconds before the end at which the automatic blend-out
/// starts; `None` means the blend-out time, so the blend ends with the montage.
#[test]
fn blend_out_lead_sets_when_the_automatic_blend_out_starts() {
    for (lead, expected) in [(None, 1.8f32), (Some(0.5), 1.5), (Some(1.0), 1.0)] {
        let start = blend_out_start(lead);
        assert!(
            (start - expected).abs() < 0.021,
            "lead {lead:?}: blend-out at {start}, expected {expected}"
        );
    }
}

/// Root motion per frame of a reverse montage with a warp window, with and without a target.
fn reverse_root_motion(target: bool) -> Vec<Vec3> {
    let mut rig = Rig::new();
    let mut m = immediate("lunge", "strike");
    m.warps = vec![WarpWindow {
        target: "enemy".into(),
        start: 0.2,
        end: 1.6,
        translation: true,
        vertical: true,
        rotation: true,
        ..WarpWindow::new("", 0.0, 0.0)
    }];
    rig.player.play(
        Arc::new(m),
        PlayOptions {
            reverse: true,
            ..PlayOptions::default()
        },
    );
    if target {
        rig.player
            .set_warp_target("enemy", WarpTarget::at(Vec3::new(3.0, 1.0, 4.0)));
    }
    (0..30)
        .map(|_| {
            rig.step(0.05);
            rig.root.0
        })
        .collect()
}

/// Spec 17 § 4b: reverse playback is supported "with warp windows off".
#[test]
fn reverse_playback_ignores_warp_windows() {
    let free = reverse_root_motion(false);
    let targeted = reverse_root_motion(true);
    // The reverse montage does move the root (backwards along +Z), so the comparison means
    // something.
    let travelled: Vec3 = free.iter().copied().sum();
    assert!(travelled.z > 0.5, "reverse root motion {travelled}");
    for (frame, (a, b)) in free.iter().zip(&targeted).enumerate() {
        assert!(
            (*a - *b).length() < 1e-6,
            "frame {frame}: {a} vs {b} (a warp applied)"
        );
    }
}

/// Spec 17 § 4b: "Clip notifies inside segments still fire as `AnimNotify` while the instance's
/// weight is at least one half." A notify crossed at weight 0.2 is silent, one at 0.8 fires.
#[test]
fn segment_clip_notifies_fire_only_at_half_weight_or_more() {
    let mut rig = Rig::new();
    let mut m = Montage::single("tag", "full_body", "tagged");
    m.blend_in.time = 1.0;
    m.blend_in.curve = TransitionCurve::Linear;
    // No automatic blend-out: the weight only rises over the clip.
    m.auto_blend_out = false;
    rig.player.play(Arc::new(m), PlayOptions::default());
    for _ in 0..19 {
        rig.step(0.05);
    }
    let names: Vec<&str> = rig.notifies.iter().map(|n| n.name.as_str()).collect();
    assert_eq!(
        names,
        ["late"],
        "early fired at weight 0.2 or late did not fire at 0.8"
    );
}

fn setup(roles: Vec<(String, SyncRole)>) -> SyncSetup {
    SyncSetup {
        format_version: regia_animation::SYNC_FORMAT_VERSION,
        groups: vec![SyncGroupDef {
            name: "gait".into(),
            roles,
        }],
        markers: Vec::new(),
    }
}

fn elect(setup: &SyncSetup, members: &[(&str, f32)]) -> Option<String> {
    let mut groups = SyncGroups::default();
    groups.begin_frame();
    for &(clip, weight) in members {
        groups.record(
            setup,
            "gait",
            SyncMember {
                clip,
                weight,
                time: 0.1,
                speed: 1.0,
                looping: true,
                duration: 1.0,
            },
        );
    }
    groups.end_frame();
    groups.groups()[0].leader.as_ref().map(|l| l.clip.clone())
}

/// Spec 17 § 4b sync groups: a `NeverLead` member never leads while a `MayLead` member plays,
/// however heavy it is; when every member is `NeverLead`, the group still has a leader.
#[test]
fn never_lead_members_lead_only_when_nothing_else_can() {
    let walk_never = setup(vec![("walk".into(), SyncRole::NeverLead)]);
    assert_eq!(
        elect(&walk_never, &[("walk", 0.9), ("run", 0.1)]).as_deref(),
        Some("run")
    );
    // Without the role the heavier clip leads (control).
    let plain = setup(Vec::new());
    assert_eq!(
        elect(&plain, &[("walk", 0.9), ("run", 0.1)]).as_deref(),
        Some("walk")
    );
    // Every member NeverLead: someone still leads, so followers have a clock.
    let both_never = setup(vec![
        ("walk".into(), SyncRole::NeverLead),
        ("run".into(), SyncRole::NeverLead),
    ]);
    assert!(elect(&both_never, &[("walk", 0.9), ("run", 0.1)]).is_some());
}

/// Spec 17 § 3: blend-space "non-finite inputs read as 0" — the weights equal those at 0.
#[test]
fn a_non_finite_blend_space_input_reads_as_zero() {
    let one = BlendSpace1D {
        param: "speed".into(),
        samples: [(-1.0, "back"), (0.5, "walk"), (2.0, "run")]
            .iter()
            .map(|&(value, clip)| BlendSample1D {
                value,
                clip: clip.into(),
            })
            .collect(),
    };
    for v in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        assert_eq!(one.weights(v), one.weights(0.0), "1D input {v}");
    }
    let mut two = BlendSpace2D {
        param_x: "x".into(),
        param_y: "y".into(),
        samples: [
            ([-1.0, -1.0], "a"),
            ([1.5, -1.0], "b"),
            ([0.3, 1.0], "c"),
            ([1.0, 1.2], "d"),
        ]
        .iter()
        .map(|&(point, clip)| BlendSample2D {
            point,
            clip: clip.into(),
        })
        .collect(),
        triangles: Vec::new(),
    };
    two.triangulate();
    for (bad, good) in [
        ([f32::NAN, 0.4], [0.0, 0.4]),
        ([0.4, f32::INFINITY], [0.4, 0.0]),
        ([f32::NAN, f32::NEG_INFINITY], [0.0, 0.0]),
    ] {
        assert_eq!(two.weights(bad), two.weights(good), "2D input {bad:?}");
    }
}

/// Spec 17 § 4a: default eye limits 35° in, 40° out, 26° up, 31° down.
#[test]
fn eye_limit_defaults_are_the_spec_angles() {
    let limits = EyeLimits::default();
    for (what, got, degrees) in [
        ("inward", limits.inward, 35.0f32),
        ("outward", limits.outward, 40.0),
        ("up", limits.up, 26.0),
        ("down", limits.down, 31.0),
    ] {
        assert!(
            (got - degrees.to_radians()).abs() < 1e-6,
            "{what}: {} degrees, spec {degrees}",
            got.to_degrees()
        );
    }
}

/// Spec 17 § 2: the pose is written to the joints' `Transform` in `PostUpdate/AnimationApply`,
/// before transform propagation, so a joint's `GlobalTransform` shows the same frame's pose
/// (no one-frame lag for sockets, attachments and skinning).
#[test]
fn animation_apply_runs_before_transform_propagation() {
    use regia_animation::AnimationPlugin;
    use regia_animation::components::{Animator, spawn_skeleton};
    use regia_scene::{GlobalTransform, Transform};
    use std::time::Duration;

    let mut app = regia_app::App::new();
    app.add_plugins((
        regia_app::MinimalPlugins,
        regia_scene::ScenePlugin,
        AnimationPlugin,
    ));
    app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
        Duration::from_millis(50),
    ));
    app.update();
    // The root turns about Y and the arm slides along X, continuously, over a 2 s loop.
    let mut clip = AnimationClip::empty("turn", 2.0);
    clip.tracks.push(JointTrack {
        joint: 0,
        translations: TrackCurve::Static(Vec3::ZERO),
        rotations: TrackCurve::Animated {
            times: vec![0.0, 1.0, 2.0],
            values: vec![Quat::IDENTITY, Quat::from_rotation_y(1.0), Quat::IDENTITY],
            exact_rotation: false,
        },
        scales: TrackCurve::Static(Vec3::ONE),
    });
    clip.tracks.push(JointTrack {
        joint: 1,
        translations: TrackCurve::Animated {
            times: vec![0.0, 1.0, 2.0],
            values: vec![Vec3::Y, Vec3::new(1.0, 1.0, 0.0), Vec3::Y],
            exact_rotation: false,
        },
        rotations: TrackCurve::Static(Quat::IDENTITY),
        scales: TrackCurve::Static(Vec3::ONE),
    });
    let binding = spawn_skeleton(app.world_mut(), Arc::new(skeleton()));
    let joints = binding.joints.clone();
    app.world_mut()
        .spawn((Transform::default(), binding, Animator::single(clip, true)));
    let mut moved = false;
    let mut last = Vec3::ZERO;
    for frame in 0..10 {
        app.update();
        for &joint in &joints {
            let local = app
                .world()
                .get::<Transform>(joint)
                .expect("joint")
                .compute_affine();
            let global = app.world().get::<GlobalTransform>(joint).expect("joint global").0;
            assert!(
                local.abs_diff_eq(global, 1e-5),
                "frame {frame}: joint {joint:?} global lags its pose"
            );
        }
        let arm = app
            .world()
            .get::<GlobalTransform>(joints[1])
            .expect("arm")
            .translation();
        moved |= (arm - last).length() > 1e-3 && frame > 0;
        last = arm;
    }
    assert!(moved, "the arm animates, so a lag would show");
}

/// Spec 17 § 4b: "Clips resolve against the player's own library first, then the animator's
/// graph library."
#[test]
fn montage_clips_resolve_in_the_player_library_first_then_the_graph_library() {
    let skeleton = skeleton();
    // The graph library has its own `strike` (walking +X) and a clip only it has.
    let mut graph_clips = ClipLibrary::new();
    let mut sideways = walk("strike", 2.0);
    if let TrackCurve::Animated { values, .. } = &mut sideways.tracks[0].translations {
        values[1] = Vec3::new(2.0, 0.0, 0.0);
    }
    graph_clips.insert(sideways);
    graph_clips.insert(walk("graph_only", 2.0));
    let travel = |montage: Montage| -> (Vec3, Vec<String>) {
        let mut player = MontagePlayer::new(vec![SlotDef::full_body("full_body", "body")], library());
        player.play(Arc::new(montage), PlayOptions::default());
        let mut sum = Vec3::ZERO;
        let mut kinds = Vec::new();
        let mut notifies = Vec::new();
        for _ in 0..10 {
            let mut pose = skeleton.bind_pose_pose();
            let mut root = (Vec3::ZERO, Quat::IDENTITY);
            let frame = MontageFrame {
                skeleton: &skeleton,
                graph_clips: Some(&graph_clips),
                delta: 0.05,
                rig: RigPlacement::default(),
            };
            player.evaluate(&frame, &mut pose, &mut root, &mut notifies);
            sum += root.0;
            kinds.extend(player.take_events().into_iter().map(|e| format!("{:?}", e.kind)));
        }
        (sum, kinds)
    };
    // `strike` is in both: the player's own (walking −Z) wins.
    let (own, _) = travel(immediate("a", "strike"));
    assert!(own.z < -0.4 && own.x.abs() < 1e-4, "player library first: {own}");
    // `graph_only` is found in the graph library and plays (not ended at once as unresolved).
    let (fallback, kinds) = travel(immediate("b", "graph_only"));
    assert!(fallback.z < -0.4, "graph library fallback: {fallback}");
    assert!(
        !kinds.iter().any(|k| k.contains("Ended")),
        "an unresolved montage ends at once: {kinds:?}"
    );
}
