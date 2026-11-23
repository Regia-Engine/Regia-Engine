// SPDX-License-Identifier: MPL-2.0
//! Montage playback through the public API (spec 17 § 4b, ADR-1465): sections and links,
//! branching points, notify and window order, interruption / blend-out / completion events,
//! queueing, slot masks, root-motion windows and motion warping onto a target.
//!
//! The player tests drive [`MontagePlayer::evaluate`] directly with fixed frame times; the warp
//! and message tests run through `AnimationPlugin`. No wall clock: every input is fixed.

use std::sync::Arc;
use std::time::Duration;

use regia_animation::clip::{AnimationClip, JointTrack, Notify, RootMotionMode, RootMotionTrack, TrackCurve};
use regia_animation::components::spawn_skeleton;
use regia_animation::montage::{MontageNotify, MontageSection, SectionNext, TimeWindow, WarpWindow};
use regia_animation::montage_player::{MontageFrame, MontageRecord, RigPlacement};
use regia_animation::pose::{Pose, translated};
use regia_animation::{
    AnimationPlugin, Animator, BranchAction, ClipLibrary, Montage, MontageEvent, MontageEventKind,
    MontagePhase, MontagePlayer, PlayOptions, RootMotionOutput, Skeleton, SlotDef, WarpTarget,
};
use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_ecs::ecs::message::{MessageCursor, Messages};
use regia_scene::Transform;

fn skeleton() -> Skeleton {
    Skeleton::from_local_bind(
        vec!["root".into(), "arm".into()],
        vec![None, Some(0)],
        vec![translated(Vec3::ZERO), translated(Vec3::Y)],
    )
    .expect("valid skeleton")
}

/// A 2 s clip: the root walks 1 m/s toward −Z (XZ root motion), the arm rises from y 1 to 3.
fn strike(name: &str, duration: f32) -> AnimationClip {
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
    clip.tracks.push(JointTrack {
        joint: 1,
        translations: TrackCurve::Animated {
            times: vec![0.0, duration],
            values: vec![Vec3::Y, Vec3::new(0.0, 3.0, 0.0)],
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
    clips.insert(strike("strike", 2.0));
    let mut tagged = strike("tagged", 1.0);
    tagged.notifies.push(Notify {
        time: 0.5,
        name: "whoosh".into(),
        payload: None,
    });
    clips.insert(tagged);
    clips
}

/// Sections: `windup` 0–0.5 (continue), `loop` 0.5–1.0 (loops itself), `finish` 1.0–2.0 (stop).
fn combo() -> Montage {
    let mut m = Montage::single("combo", "full_body", "strike");
    m.blend_in.time = 0.0;
    m.blend_out.time = 0.0;
    m.sections = vec![
        MontageSection {
            follow: None,
            name: "windup".into(),
            start: 0.0,
            next: SectionNext::Continue,
        },
        MontageSection {
            follow: None,
            name: "loop".into(),
            start: 0.5,
            next: SectionNext::Jump("loop".into()),
        },
        MontageSection {
            follow: None,
            name: "finish".into(),
            start: 1.0,
            next: SectionNext::Stop,
        },
    ];
    m
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
            player: MontagePlayer::new(
                vec![
                    SlotDef::full_body("full_body", "body"),
                    SlotDef::masked("upper", "upper", &["arm"]),
                ],
                library(),
            ),
            pose,
            root: (Vec3::ZERO, Quat::IDENTITY),
            events: Vec::new(),
            notifies: Vec::new(),
        }
    }

    /// One frame over a graph that holds the bind pose and has no root motion.
    fn step(&mut self, dt: f32) {
        self.step_at(dt, RigPlacement::default());
    }

    fn step_at(&mut self, dt: f32, rig: RigPlacement) {
        self.pose = self.skeleton.bind_pose_pose();
        self.root = (Vec3::ZERO, Quat::IDENTITY);
        let frame = MontageFrame {
            skeleton: &self.skeleton,
            graph_clips: None,
            delta: dt,
            rig,
        };
        self.player
            .evaluate(&frame, &mut self.pose, &mut self.root, &mut self.notifies);
        self.events.extend(self.player.take_events());
    }

    fn kinds(&self) -> Vec<String> {
        self.events.iter().map(describe).collect()
    }
}

fn describe(record: &MontageRecord) -> String {
    match &record.kind {
        MontageEventKind::Started => "started".into(),
        MontageEventKind::Section { from, to } => format!("section {from}->{to}"),
        MontageEventKind::Notify { name, .. } => format!("notify {name}"),
        MontageEventKind::WindowBegin { name, .. } => format!("begin {name}"),
        MontageEventKind::WindowEnd { name } => format!("end {name}"),
        MontageEventKind::BranchPoint { name } => format!("branch {name}"),
        MontageEventKind::BlendingOut { interrupted } => format!("blend_out {interrupted}"),
        MontageEventKind::Ended { interrupted } => format!("ended {interrupted}"),
    }
}

#[test]
fn sections_follow_their_links_and_runtime_relinks() {
    let mut rig = Rig::new();
    let id = rig.player.play(Arc::new(combo()), PlayOptions::default());
    // 0.25 s steps: windup → loop, then loop → loop twice.
    for _ in 0..8 {
        rig.step(0.25);
    }
    let status = rig.player.status(id).expect("still playing");
    assert_eq!(status.section, "loop", "a self-jump loops the section");
    assert!(status.time >= 0.5 && status.time < 1.0, "{status:?}");
    // Relink the loop to the finish; the montage then stops at the finish's end.
    assert!(rig.player.set_next_section(None, "loop", SectionNext::Continue));
    for _ in 0..12 {
        rig.step(0.25);
    }
    assert!(rig.player.status(id).is_none(), "ended");
    let kinds = rig.kinds();
    assert_eq!(kinds.first().map(String::as_str), Some("started"));
    assert_eq!(
        kinds.iter().filter(|k| *k == "section loop->loop").count(),
        3,
        "{kinds:?}"
    );
    let tail: Vec<&str> = kinds.iter().rev().take(3).rev().map(String::as_str).collect();
    assert_eq!(
        tail,
        ["section loop->finish", "blend_out false", "ended false"],
        "{kinds:?}"
    );
}

#[test]
fn playback_is_deterministic_and_frame_rate_independent() {
    let run = |dt: f32, frames: usize| {
        let mut rig = Rig::new();
        let mut m = combo();
        m.notifies.push(MontageNotify {
            follow: None,
            name: "combo_window".into(),
            time: 0.7,
            duration: 0.0,
            payload: None,
            branch: true,
        });
        let id = rig.player.play(Arc::new(m), PlayOptions::default());
        assert!(
            rig.player
                .on_branch(Some(id), "combo_window", BranchAction::JumpTo("finish".into()))
        );
        for _ in 0..frames {
            rig.step(dt);
        }
        (rig.kinds(), rig.player.status(id).map(|s| (s.section, s.time)))
    };
    // 1.3 s of playback: windup, loop to 0.7 (0.7 s), branch to finish at 1.0, then 0.6 s more.
    let (coarse, at_coarse) = run(0.1, 13);
    let (fine, at_fine) = run(0.025, 52);
    assert_eq!(coarse, fine, "same events whatever the frame time");
    let (section, time) = at_coarse.expect("playing");
    let (_, fine_time) = at_fine.expect("playing");
    assert_eq!(section, "finish");
    assert!(
        (time - 1.6).abs() < 1e-4 && (fine_time - 1.6).abs() < 1e-4,
        "{time} {fine_time}"
    );
    assert!(
        coarse
            .windows(2)
            .any(|w| w[0] == "branch combo_window" && w[1] == "section loop->finish"),
        "the armed jump applies at the branching point: {coarse:?}"
    );
    // The same run twice gives identical results.
    assert_eq!(run(0.1, 13).0, coarse);
}

#[test]
fn notifies_windows_and_branch_points_fire_in_timeline_order() {
    let mut rig = Rig::new();
    let mut m = Montage::single("hit", "full_body", "tagged");
    m.blend_in.time = 0.0;
    m.notifies = vec![
        MontageNotify {
            follow: None,
            name: "late".into(),
            time: 0.8,
            duration: 0.0,
            payload: Some("(power: 2)".into()),
            branch: false,
        },
        MontageNotify {
            follow: None,
            name: "hitbox".into(),
            time: 0.2,
            duration: 0.4,
            payload: None,
            branch: false,
        },
        MontageNotify {
            follow: None,
            name: "start".into(),
            time: 0.0,
            duration: 0.0,
            payload: None,
            branch: false,
        },
        MontageNotify {
            follow: None,
            name: "cancel".into(),
            time: 0.3,
            duration: 0.0,
            payload: None,
            branch: true,
        },
    ];
    rig.player.play(Arc::new(m), PlayOptions::default());
    // One big step crosses everything; order must still be timeline order.
    rig.step(0.9);
    let kinds = rig.kinds();
    assert_eq!(
        kinds,
        [
            "started",
            "notify start",
            "begin hitbox",
            "branch cancel",
            "end hitbox",
            "notify late",
            "blend_out false",
        ],
        "{kinds:?}"
    );
    // The segment's own clip notify fired through the AnimNotify path.
    assert_eq!(
        rig.notifies.iter().map(|n| n.name.as_str()).collect::<Vec<_>>(),
        ["whoosh"]
    );
}

#[test]
fn a_jump_out_of_an_open_window_closes_it() {
    let mut rig = Rig::new();
    let mut m = combo();
    m.notifies.push(MontageNotify {
        follow: None,
        name: "armor".into(),
        time: 0.1,
        duration: 0.3,
        payload: None,
        branch: false,
    });
    rig.player.play(Arc::new(m), PlayOptions::default());
    rig.step(0.2);
    assert!(rig.player.jump_to_section(None, "finish"));
    rig.step(0.1);
    let kinds = rig.kinds();
    assert_eq!(
        kinds,
        ["started", "begin armor", "section windup->finish", "end armor"],
        "{kinds:?}"
    );
}

#[test]
fn a_newer_montage_in_the_group_interrupts_and_completion_is_reported() {
    let mut rig = Rig::new();
    let mut first = Montage::single("first", "full_body", "strike");
    first.blend_in.time = 0.0;
    let mut second = Montage::single("second", "full_body", "tagged");
    second.blend_in.time = 0.2;
    second.blend_out.time = 0.2;
    let a = rig.player.play(Arc::new(first), PlayOptions::default());
    rig.step(0.1);
    let b = rig.player.play(Arc::new(second), PlayOptions::default());
    rig.step(0.1);
    let statuses = rig.player.instances();
    assert_eq!(statuses.len(), 2, "both blend during the interruption");
    assert_eq!(statuses[0].phase, MontagePhase::BlendingOut);
    assert!(statuses[0].interrupted);
    assert!(
        (statuses[0].weight + statuses[1].weight - 1.0).abs() < 1e-4,
        "{statuses:?}"
    );
    for _ in 0..20 {
        rig.step(0.1);
    }
    assert!(rig.player.instances().is_empty());
    assert_eq!(of(&rig.events, a), ["started", "blend_out true", "ended true"]);
    assert_eq!(of(&rig.events, b), ["started", "blend_out false", "ended false"]);
    // An explicit stop is an interruption too.
    let c = rig.player.play(Arc::new(combo()), PlayOptions::default());
    rig.step(0.1);
    assert_eq!(rig.player.stop(None, Some(0.0)), 1);
    rig.step(0.1);
    assert_eq!(of(&rig.events, c), ["started", "blend_out true", "ended true"]);
}

fn of(events: &[MontageRecord], id: regia_animation::MontageId) -> Vec<String> {
    events.iter().filter(|e| e.id == id).map(describe).collect()
}

#[test]
fn a_queued_montage_starts_when_the_current_one_blends_out() {
    let mut rig = Rig::new();
    let mut first = Montage::single("first", "full_body", "tagged");
    first.blend_out.time = 0.25;
    let next = Montage::single("next", "full_body", "tagged");
    let a = rig.player.play(Arc::new(first), PlayOptions::default());
    let b = rig.player.queue(Arc::new(next), PlayOptions::default());
    assert_eq!(rig.player.queued(), vec![b]);
    // `first` is 1 s long; its automatic blend-out starts at 0.75 s.
    for _ in 0..7 {
        rig.step(0.1);
    }
    assert_eq!(rig.player.queued(), vec![b], "still waiting at 0.7 s");
    rig.step(0.1);
    assert!(rig.player.queued().is_empty(), "started with the blend-out");
    assert!(
        rig.player
            .status(a)
            .is_some_and(|s| s.phase == MontagePhase::BlendingOut)
    );
    assert!(rig.player.status(b).is_some());
}

#[test]
fn a_masked_slot_overrides_only_its_joints_and_drops_root_motion() {
    let mut rig = Rig::new();
    let mut m = Montage::single("wave", "upper", "strike");
    m.blend_in.time = 0.0;
    rig.player.play(Arc::new(m), PlayOptions::default());
    rig.step(1.0);
    // The arm follows the clip (y 1 → 3 over 2 s: 2 at 1 s); the root keeps the graph pose.
    assert!(
        (rig.pose.translations[1].y - 2.0).abs() < 1e-4,
        "{:?}",
        rig.pose.translations
    );
    assert!(rig.pose.translations[0].length() < 1e-6);
    // The upper slot does not cover the skeleton root, so it adds no root motion.
    assert!(rig.root.0.length() < 1e-6, "{:?}", rig.root);
}

#[test]
fn root_motion_off_windows_drop_their_share() {
    let mut rig = Rig::new();
    let mut m = Montage::single("dash", "full_body", "strike");
    m.blend_in.time = 0.0;
    m.root_motion_off = vec![TimeWindow {
        start: 0.25,
        end: 0.75,
    }];
    rig.player.play(Arc::new(m), PlayOptions::default());
    let mut travelled = Vec3::ZERO;
    for _ in 0..4 {
        rig.step(0.25);
        travelled += rig.root.0;
    }
    // 1 s at 1 m/s minus the 0.5 s window.
    assert!(
        (travelled - Vec3::new(0.0, 0.0, -0.5)).length() < 1e-4,
        "{travelled}"
    );
    // The full-body montage pins the root in place (its motion goes to the controller).
    assert!(rig.pose.translations[0].length() < 1e-5);
}

#[test]
fn warping_lands_on_the_target_and_tracks_it_when_it_moves() {
    for moved in [false, true] {
        let mut rig = Rig::new();
        let mut m = Montage::single("lunge", "full_body", "strike");
        m.blend_in.time = 0.0;
        m.warps = vec![WarpWindow {
            target: "enemy".into(),
            start: 0.2,
            end: 1.6,
            translation: true,
            vertical: false,
            rotation: true,
            ..WarpWindow::new("", 0.0, 0.0)
        }];
        rig.player.play(Arc::new(m), PlayOptions::default());
        let mut target = Vec3::new(1.5, 0.0, -2.5);
        rig.player.set_warp_target("enemy", WarpTarget::at(target));
        let mut placement = RigPlacement::default();
        let dt = 0.05;
        // 1.6 s = 32 frames reaches the window end exactly.
        for frame in 0..32 {
            if moved && frame == 16 {
                target = Vec3::new(2.0, 0.0, -2.0);
                rig.player.set_warp_target("enemy", WarpTarget::at(target));
            }
            rig.step_at(dt, placement);
            // The test plays the character controller: apply the root motion.
            placement.position += placement.rotation * rig.root.0;
            placement.rotation = (placement.rotation * rig.root.1).normalize();
        }
        let miss = (placement.position - target).length();
        assert!(miss < 0.01, "moved {moved}: ended {} from the target", miss);
        // Facing: −Z turned toward the target's direction from the start (rotation warp).
        assert!(placement.rotation.is_finite());
    }
}

/// Through the plugin: messages reach readers and a warped lunge moves a root-motion rig
/// (with a minimal controller system) onto its target.
#[test]
fn plugin_writes_montage_messages_and_warps_a_rig() {
    let dt = 1.0 / 32.0;
    let mut app = App::new();
    app.add_plugins((regia_app::MinimalPlugins, AnimationPlugin));
    app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
        Duration::from_secs_f32(dt),
    ));
    app.add_systems(
        Update,
        apply_root_motion.after(regia_animation::systems::evaluate_animators),
    );
    let skeleton = Arc::new(skeleton());
    let rig = app.world_mut().spawn(Transform::default()).id();
    let binding = spawn_skeleton(app.world_mut(), skeleton);
    let idle = AnimationClip::empty("idle", 1.0);
    let mut player = MontagePlayer::new(vec![SlotDef::full_body("full_body", "body")], library());
    let mut m = Montage::single("lunge", "full_body", "strike");
    m.warps = vec![WarpWindow {
        target: "enemy".into(),
        start: 0.25,
        end: 1.5,
        translation: true,
        vertical: false,
        rotation: false,
        ..WarpWindow::new("", 0.0, 0.0)
    }];
    player.add_montage(m);
    let target = Vec3::new(-1.0, 0.0, -3.0);
    player.set_warp_target("enemy", WarpTarget::at(target));
    let id = player.play_named("lunge", PlayOptions::default());
    assert!(id.is_some());
    app.world_mut()
        .entity_mut(rig)
        .insert((binding, Animator::single(idle, true), player));

    let mut seen: Vec<String> = Vec::new();
    let mut at_window_end = None;
    let mut cursor = MessageCursor::<MontageEvent>::default();
    for _ in 0..120 {
        app.update();
        for event in cursor.read(app.world().resource::<Messages<MontageEvent>>()) {
            assert_eq!(event.entity, rig);
            seen.push(event.kind.as_str().to_string());
        }
        let player = app.world().get::<MontagePlayer>(rig).expect("player");
        if at_window_end.is_none() && player.instances().first().is_some_and(|s| s.time >= 1.5 - 1e-4) {
            at_window_end = app.world().get::<Transform>(rig).map(|t| t.translation);
        }
    }
    assert!(seen.contains(&"started".to_string()), "{seen:?}");
    assert!(seen.contains(&"ended".to_string()), "{seen:?}");
    let reached = at_window_end.expect("the lunge reached its window end");
    assert!(
        (reached - target).length() < 0.02,
        "rig at {reached}, target {target}"
    );
}

/// The controller stand-in: drain the rig's root motion into its transform every frame.
fn apply_root_motion(mut rigs: Query<(&mut Transform, &mut RootMotionOutput)>) {
    for (mut transform, mut output) in &mut rigs {
        let (translation, rotation, _) = output.take();
        let step = transform.rotation * (transform.scale * translation);
        transform.translation += step;
        transform.rotation = (transform.rotation * rotation).normalize();
    }
}

#[test]
fn without_auto_blend_out_the_last_pose_holds_until_stopped() {
    let mut rig = Rig::new();
    let mut m = Montage::single("pose", "full_body", "tagged");
    m.blend_in.time = 0.0;
    m.auto_blend_out = false;
    let id = rig.player.play(Arc::new(m), PlayOptions::default());
    for _ in 0..30 {
        rig.step(0.1);
    }
    let status = rig.player.status(id).expect("held");
    assert_eq!(status.phase, MontagePhase::Holding);
    assert!(
        (status.time - 1.0).abs() < 1e-5 && status.weight == 1.0,
        "{status:?}"
    );
    // The held pose is the clip's last frame (arm at y 3).
    assert!((rig.pose.translations[1].y - 3.0).abs() < 1e-4);
    assert_eq!(rig.player.stop(Some(id), Some(0.0)), 1);
    rig.step(0.1);
    assert!(rig.player.status(id).is_none());
    assert_eq!(of(&rig.events, id), ["started", "blend_out true", "ended true"]);
}

#[test]
fn play_rate_and_set_rate_scale_montage_time() {
    let mut rig = Rig::new();
    let mut m = Montage::single("fast", "full_body", "strike");
    m.rate = 2.0;
    let id = rig.player.play(
        Arc::new(m),
        PlayOptions {
            rate: 0.5,
            section: None,
            start_time: 0.25,
            ..PlayOptions::default()
        },
    );
    rig.step(0.5);
    // 0.25 start + 0.5 s × (2 × 0.5).
    assert!(
        rig.player
            .status(id)
            .is_some_and(|s| (s.time - 0.75).abs() < 1e-5)
    );
    assert!(rig.player.set_rate(Some(id), 3.0));
    rig.step(0.1);
    // + 0.1 s × (2 × 3).
    assert!(
        rig.player
            .status(id)
            .is_some_and(|s| (s.time - 1.35).abs() < 1e-5)
    );
}

#[test]
fn vertical_warping_reaches_the_target_height() {
    let mut rig = Rig::new();
    let mut m = Montage::single("vault", "full_body", "strike");
    m.blend_in.time = 0.0;
    m.warps = vec![WarpWindow {
        target: "ledge".into(),
        start: 0.0,
        end: 1.0,
        translation: true,
        vertical: true,
        rotation: false,
        ..WarpWindow::new("", 0.0, 0.0)
    }];
    rig.player.play(Arc::new(m), PlayOptions::default());
    let target = Vec3::new(0.0, 1.2, -1.5);
    rig.player.set_warp_target("ledge", WarpTarget::at(target));
    let mut placement = RigPlacement::default();
    for _ in 0..10 {
        rig.step_at(0.1, placement);
        placement.position += placement.rotation * rig.root.0;
    }
    assert!(
        (placement.position - target).length() < 1e-3,
        "{}",
        placement.position
    );
}

/// A montage with a `sync_group` leads that group while it blends out: the graph's clip in the
/// group lines up with the montage's clip phase before it takes over again.
#[test]
fn a_blending_out_montage_leads_its_sync_group() {
    use regia_animation::debug::NodeDebugState;
    use regia_animation::graph::{AnimGraph, AnimNode, ClipNode};
    use regia_animation::sync::{SyncGroupDef, SyncSetup};

    let dt = 1.0 / 30.0;
    let mut app = App::new();
    app.add_plugins((regia_app::MinimalPlugins, AnimationPlugin));
    app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
        Duration::from_secs_f32(dt),
    ));
    let skeleton = Arc::new(skeleton());
    let rig = app.world_mut().spawn(Transform::default()).id();
    let binding = spawn_skeleton(app.world_mut(), skeleton);
    let mut clips = ClipLibrary::new();
    clips.insert(AnimationClip::empty("walk", 1.0));
    clips.insert(AnimationClip::empty("run", 0.6));
    let graph = AnimGraph {
        parameters: Vec::new(),
        root: AnimNode::Clip(ClipNode {
            clip: "walk".into(),
            looping: true,
            speed: 1.0,
            sync_group: Some("gait".into()),
        }),
    };
    let mut animator = Animator::graph(graph, clips);
    animator.set_sync_setup(Some(Arc::new(SyncSetup {
        format_version: regia_animation::SYNC_FORMAT_VERSION,
        groups: vec![SyncGroupDef {
            name: "gait".into(),
            roles: Vec::new(),
        }],
        markers: Vec::new(),
    })));
    let mut player = MontagePlayer::default();
    let mut dash = Montage::single("dash", "full_body", "run");
    dash.tracks[0].segments[0].loops = 4;
    dash.blend_out.time = 0.6;
    dash.sync_group = Some("gait".into());
    let id = player.play(Arc::new(dash), PlayOptions::default());
    app.world_mut()
        .entity_mut(rig)
        .insert((binding, animator, player));

    let walk_time = |app: &App| {
        let animator = app.world().get::<Animator>(rig)?;
        animator
            .debug_snapshot()?
            .nodes
            .iter()
            .find_map(|n| match &n.state {
                NodeDebugState::Clip { time, .. } => Some(*time),
                _ => None,
            })
    };
    let mut checked = 0;
    for _ in 0..90 {
        app.update();
        let Some(status) = app.world().get::<MontagePlayer>(rig).and_then(|p| p.status(id)) else {
            break;
        };
        // Blending out (from 2.4 − 0.6 = 1.8 s): two frames to hand the lead over, then in step.
        if status.phase == MontagePhase::BlendingOut && status.time > 1.8 + 3.0 * dt {
            let run_phase = (status.time / 0.6).fract();
            let walk_phase = walk_time(&app).expect("walk clip").rem_euclid(1.0);
            let gap = (run_phase - walk_phase).rem_euclid(1.0);
            assert!(
                gap.min(1.0 - gap) < 0.02,
                "walk {walk_phase} montage run {run_phase}"
            );
            checked += 1;
        }
    }
    assert!(checked > 5, "the blend-out was observed ({checked} frames)");
}

/// Montages on different slot groups play together (an upper-body reload over a full-body
/// emote's legs); one in the same group as a playing one replaces it.
#[test]
fn different_slot_groups_play_together_and_one_group_interrupts() {
    let mut rig = Rig::new();
    let mut emote = Montage::single("emote", "full_body", "strike");
    emote.blend_in.time = 0.0;
    let mut reload = Montage::single("reload", "upper", "tagged");
    reload.blend_in.time = 0.0;
    let a = rig.player.play(Arc::new(emote), PlayOptions::default());
    let b = rig.player.play(Arc::new(reload.clone()), PlayOptions::default());
    rig.step(0.5);
    let both = rig.player.instances();
    assert!(both.iter().all(|s| s.phase == MontagePhase::Playing), "{both:?}");
    // The upper slot (newer) owns the arm: tagged lifts it 1 → 3 over 1 s, so 2 at 0.5 s; the
    // full-body strike would give 1.5.
    assert!(
        (rig.pose.translations[1].y - 2.0).abs() < 1e-4,
        "{:?}",
        rig.pose.translations
    );
    // The full-body montage still drives the root motion (the upper slot does not cover the root).
    assert!(
        (rig.root.0 - Vec3::new(0.0, 0.0, -0.5)).length() < 1e-4,
        "{:?}",
        rig.root
    );
    // A second upper-body montage interrupts only the first reload (it blends out over the new
    // one's zero blend-in, so it is gone after one frame).
    let c = rig.player.play(Arc::new(reload), PlayOptions::default());
    rig.step(0.05);
    assert!(
        rig.player
            .status(a)
            .is_some_and(|s| s.phase == MontagePhase::Playing)
    );
    assert_eq!(of(&rig.events, b), ["started", "blend_out true", "ended true"]);
    assert!(rig.player.status(c).is_some());
}

/// A montage whose clip is missing ends at once as interrupted instead of playing garbage.
#[test]
fn a_montage_with_a_missing_clip_ends_as_interrupted() {
    let mut rig = Rig::new();
    let id = rig.player.play(
        Arc::new(Montage::single("ghost", "full_body", "absent")),
        PlayOptions::default(),
    );
    let before = rig.skeleton.bind_pose_pose();
    rig.step(0.1);
    assert!(rig.player.status(id).is_none());
    assert_eq!(of(&rig.events, id), ["started", "blend_out true", "ended true"]);
    assert_eq!(
        rig.pose.translations, before.translations,
        "the graph pose is untouched"
    );
}

/// Hand-run timing, not part of the gate (no assertion on time): the montage stage for 100 rigs
/// of 60 joints with two tracks each. Run with
/// `cargo test -p regia_animation --release --test montage -- --ignored --nocapture`.
#[test]
#[ignore = "hand-run timing"]
fn montage_stage_cost_hand_run() {
    const JOINTS: usize = 60;
    const RIGS: usize = 100;
    const FRAMES: usize = 300;
    let names: Vec<String> = (0..JOINTS).map(|i| format!("j{i}")).collect();
    let parents: Vec<Option<u32>> = (0..JOINTS).map(|i| i.checked_sub(1).map(|p| p as u32)).collect();
    let binds = (0..JOINTS).map(|_| translated(Vec3::Y * 0.1)).collect();
    let skeleton = Skeleton::from_local_bind(names, parents, binds).expect("skeleton");
    let mut clip = strike("big", 2.0);
    clip.tracks = (0..JOINTS as u32)
        .map(|joint| JointTrack {
            joint,
            translations: TrackCurve::Animated {
                times: vec![0.0, 1.0, 2.0],
                values: vec![Vec3::Y * 0.1, Vec3::new(0.02, 0.1, 0.0), Vec3::Y * 0.1],
                exact_rotation: false,
            },
            rotations: TrackCurve::Animated {
                times: vec![0.0, 2.0],
                values: vec![Quat::IDENTITY, Quat::from_rotation_z(0.3)],
                exact_rotation: false,
            },
            scales: TrackCurve::Static(Vec3::ONE),
        })
        .collect();
    let mut clips = ClipLibrary::new();
    clips.insert(clip);
    let mut montage = Montage::single("loop", "full_body", "big");
    montage.tracks.push(regia_animation::montage::MontageTrack {
        slot: "upper".into(),
        segments: vec![regia_animation::montage::MontageSegment {
            clip: "big".into(),
            start: 0.0,
            end: None,
            rate: 1.0,
            loops: 1,
        }],
    });
    montage.sections = vec![MontageSection {
        follow: None,
        name: "loop".into(),
        start: 0.0,
        next: SectionNext::Jump("loop".into()),
    }];
    let montage = Arc::new(montage);
    let mut players: Vec<MontagePlayer> = (0..RIGS)
        .map(|_| {
            let mut p = MontagePlayer::new(
                vec![
                    SlotDef::full_body("full_body", "body"),
                    SlotDef::masked("upper", "upper", &["j30"]),
                ],
                clips.clone(),
            );
            p.play(montage.clone(), PlayOptions::default());
            p
        })
        .collect();
    let mut pose = skeleton.bind_pose_pose();
    let mut notifies = Vec::new();
    let start = std::time::Instant::now();
    for _ in 0..FRAMES {
        for player in &mut players {
            let mut root = (Vec3::ZERO, Quat::IDENTITY);
            let frame = MontageFrame {
                skeleton: &skeleton,
                graph_clips: None,
                delta: 1.0 / 60.0,
                rig: RigPlacement::default(),
            };
            player.evaluate(&frame, &mut pose, &mut root, &mut notifies);
            let _ = player.take_events();
        }
    }
    let per = start.elapsed().as_secs_f64() * 1e6 / (RIGS * FRAMES) as f64;
    println!(
        "montage stage: {per:.2} us per rig-frame ({JOINTS} joints, 2 tracks); {RIGS} rigs = {:.3} ms/frame",
        per * RIGS as f64 / 1000.0
    );
}

// Audit-gameplay zai H1 / g1 M (ADR-1809): a jump makes its landing time an inclusive start.

fn count(kinds: &[String], kind: &str) -> usize {
    kinds.iter().filter(|k| *k == kind).count()
}

#[test]
fn a_notify_and_a_window_on_a_loop_start_fire_on_every_loop() {
    let mut rig = Rig::new();
    let mut m = combo();
    m.notifies.push(MontageNotify {
        follow: None,
        name: "tick".into(),
        time: 0.5,
        duration: 0.0,
        payload: None,
        branch: false,
    });
    m.notifies.push(MontageNotify {
        follow: None,
        name: "damage".into(),
        time: 0.5,
        duration: 0.3,
        payload: None,
        branch: false,
    });
    rig.player.play(Arc::new(m), PlayOptions::default());
    for _ in 0..8 {
        rig.step(0.25);
    }
    let kinds = rig.kinds();
    // Entered once from the windup, then re-entered by each of the first two self-jumps (the
    // third jump lands at the end of the last frame).
    assert_eq!(count(&kinds, "section loop->loop"), 3, "{kinds:?}");
    assert_eq!(count(&kinds, "notify tick"), 3, "{kinds:?}");
    assert_eq!(count(&kinds, "begin damage"), 3, "{kinds:?}");
    assert_eq!(count(&kinds, "end damage"), 3, "{kinds:?}");
}

#[test]
fn a_manual_jump_fires_a_notify_on_the_destination_time() {
    let mut rig = Rig::new();
    let mut m = combo();
    m.notifies.push(MontageNotify {
        follow: None,
        name: "impact".into(),
        time: 1.0,
        duration: 0.0,
        payload: None,
        branch: false,
    });
    rig.player.play(Arc::new(m), PlayOptions::default());
    rig.step(0.2);
    assert!(rig.player.jump_to_section(None, "finish"));
    rig.step(0.1);
    rig.step(0.1);
    let kinds = rig.kinds();
    assert_eq!(count(&kinds, "notify impact"), 1, "{kinds:?}");
}

#[test]
fn a_jump_that_lands_where_playback_already_is_does_not_fire_twice() {
    let mut rig = Rig::new();
    let mut m = combo();
    // The windup jumps to `loop`, which starts exactly where the windup ends.
    m.sections[0].next = SectionNext::Jump("loop".into());
    m.notifies.push(MontageNotify {
        follow: None,
        name: "edge".into(),
        time: 0.5,
        duration: 0.0,
        payload: None,
        branch: false,
    });
    rig.player.play(Arc::new(m), PlayOptions::default());
    for _ in 0..3 {
        rig.step(0.25);
    }
    let kinds = rig.kinds();
    assert_eq!(count(&kinds, "section windup->loop"), 1, "{kinds:?}");
    assert_eq!(count(&kinds, "notify edge"), 1, "{kinds:?}");
}
