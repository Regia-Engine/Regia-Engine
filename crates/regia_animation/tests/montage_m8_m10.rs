// SPDX-License-Identifier: MPL-2.0
//! Spec 34 second step through the public API (ADR-1870): M8 montage edits with linked items,
//! reverse playback, time-stretch, and the M9 graph `Slot` node through `AnimationPlugin`.
//!
//! Every input is fixed: frame times are constants, no wall clock, no randomness.

use std::sync::Arc;
use std::time::Duration;

use regia_animation::clip::{AnimationClip, JointTrack, Notify, RootMotionMode, RootMotionTrack, TrackCurve};
use regia_animation::components::spawn_skeleton;
use regia_animation::graph::{
    ANIMGRAPH_FORMAT_VERSION, AnimGraph, AnimNode, ClipNode, JointMaskDef, LayerEntry, LayerMode, LayerNode,
    SlotNode,
};
use regia_animation::montage::{
    FollowMode, MontageNotify, MontageSection, MontageSegment, RateCurve, RateInterpolation, SectionNext,
    SegmentFollow,
};
use regia_animation::montage_edit::{EditedItem, FollowEdit, FollowModeEdit, MontageEdit, NextEdit};
use regia_animation::montage_player::{BranchAction, MontageFrame, MontageRecord, RigPlacement};
use regia_animation::params::{ParamDecl, ParamValue};
use regia_animation::pose::{Pose, translated};
use regia_animation::{
    AnimationPlugin, Animator, ClipLibrary, Montage, MontageEventKind, MontagePlayer, PlayError, PlayOptions,
    Skeleton, SkeletonSlots, SlotDef,
};
use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_scene::Transform;

fn skeleton() -> Skeleton {
    Skeleton::from_local_bind(
        vec!["root".into(), "arm".into()],
        vec![None, Some(0)],
        vec![translated(Vec3::ZERO), translated(Vec3::Y)],
    )
    .expect("valid skeleton")
}

/// A clip: the root walks 1 m/s toward −Z (XZ root motion), the arm rises from y 1 to 3.
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

/// A clip that holds the arm at height `y`.
fn hold_arm(name: &str, y: f32) -> AnimationClip {
    let mut clip = AnimationClip::empty(name, 1.0);
    clip.tracks.push(JointTrack {
        joint: 1,
        translations: TrackCurve::Static(Vec3::new(0.0, y, 0.0)),
        rotations: TrackCurve::Static(Quat::IDENTITY),
        scales: TrackCurve::Static(Vec3::ONE),
    });
    clip
}

/// A clip that holds the root at height `y`.
fn hold_root(name: &str, y: f32) -> AnimationClip {
    let mut clip = AnimationClip::empty(name, 1.0);
    clip.tracks.push(JointTrack {
        joint: 0,
        translations: TrackCurve::Static(Vec3::new(0.0, y, 0.0)),
        rotations: TrackCurve::Static(Quat::IDENTITY),
        scales: TrackCurve::Static(Vec3::ONE),
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

fn section(name: &str, start: f32) -> MontageSection {
    MontageSection {
        name: name.into(),
        start,
        next: SectionNext::Continue,
        follow: None,
    }
}

fn notify(name: &str, time: f32, duration: f32) -> MontageNotify {
    MontageNotify {
        name: name.into(),
        time,
        duration,
        payload: None,
        branch: false,
        follow: None,
    }
}

/// `strike` (2 s) with sections a 0–1, b 1–2, notifies at 0.5 and 1.5 and a window 0.8–1.2.
fn marked() -> Montage {
    let mut m = Montage::single("marked", "full_body", "strike");
    m.blend_in.time = 0.0;
    m.blend_out.time = 0.0;
    m.sections = vec![section("a", 0.0), section("b", 1.0)];
    m.notifies = vec![
        notify("early", 0.5, 0.0),
        notify("late", 1.5, 0.0),
        notify("win", 0.8, 0.4),
    ];
    m
}

struct Rig {
    skeleton: Skeleton,
    player: MontagePlayer,
    root: (Vec3, Quat),
    total: Vec3,
    events: Vec<MontageRecord>,
    notifies: Vec<Notify>,
}

impl Rig {
    fn new() -> Self {
        Rig {
            skeleton: skeleton(),
            player: MontagePlayer::new(vec![SlotDef::full_body("full_body", "body")], library()),
            root: (Vec3::ZERO, Quat::IDENTITY),
            total: Vec3::ZERO,
            events: Vec::new(),
            notifies: Vec::new(),
        }
    }

    fn step(&mut self, dt: f32) {
        let mut pose = self.skeleton.bind_pose_pose();
        self.root = (Vec3::ZERO, Quat::IDENTITY);
        let frame = MontageFrame {
            skeleton: &self.skeleton,
            graph_clips: None,
            delta: dt,
            rig: RigPlacement::default(),
        };
        self.player
            .evaluate(&frame, &mut pose, &mut self.root, &mut self.notifies);
        self.total += self.root.0;
        self.events.extend(self.player.take_events());
    }

    fn kinds(&self) -> Vec<String> {
        self.events
            .iter()
            .map(|r| match &r.kind {
                MontageEventKind::Started => "started".into(),
                MontageEventKind::Section { from, to } => format!("section {from}->{to}"),
                MontageEventKind::Notify { name, .. } => format!("notify {name}"),
                MontageEventKind::WindowBegin { name, .. } => format!("begin {name}"),
                MontageEventKind::WindowEnd { name } => format!("end {name}"),
                MontageEventKind::BranchPoint { name } => format!("branch {name}"),
                MontageEventKind::BlendingOut { interrupted } => format!("blend_out {interrupted}"),
                MontageEventKind::Ended { interrupted } => format!("ended {interrupted}"),
            })
            .collect()
    }

    fn ended_after(&mut self, dt: f32, max_frames: usize) -> Option<usize> {
        (1..=max_frames).find(|_| {
            self.step(dt);
            self.events
                .iter()
                .any(|e| matches!(e.kind, MontageEventKind::Ended { .. }))
        })
    }
}

// ---------------------------------------------------------------- M8 edits

fn seg(clip: &str, end: f32) -> MontageSegment {
    MontageSegment {
        clip: clip.into(),
        start: 0.0,
        end: Some(end),
        rate: 1.0,
        loops: 1,
    }
}

#[test]
fn edit_segment_moves_a_linked_section_and_layout_agrees() {
    let mut m = Montage::single("combo", "full_body", "strike");
    m.tracks[0].segments = vec![seg("strike", 1.0), seg("strike", 1.0)];
    m.sections = vec![
        section("windup", 0.0),
        MontageSection {
            follow: Some(SegmentFollow {
                track: 0,
                segment: 1,
                mode: FollowMode::ShiftWithStart,
            }),
            ..section("swing", 1.0)
        },
    ];
    // Lengthen segment 0 to the whole 2 s clip: the swing section follows segment 1's start.
    let report = m.edit_segment(0, 0, 0.0, Some(2.0)).expect("edit");
    assert_eq!(m.sections[1].start, 2.0);
    assert_eq!(report.moved.len(), 1);
    let clips = library();
    let layout = m.layout(|n| clips.get(n).cloned()).expect("layout");
    assert_eq!(layout.tracks[0].segments[1].at, m.sections[1].start);
    // The runtime ignores `follow`: the section starts where the file says.
    assert_eq!(m.section_start(1), 2.0);
}

#[test]
fn edit_ops_build_a_montage_and_keep_it_valid() {
    let mut m = Montage::single("built", "full_body", "strike");
    let no_lengths = |_: &str| None;
    let lengths = |c: &str| (c == "strike").then_some(2.0);
    // A section after 0 on a montage with none: the implicit `main` becomes explicit at 0.
    m.apply_edit(
        &MontageEdit::SetSection {
            name: "recover".into(),
            start: Some(1.5),
            next: Some(NextEdit::Stop),
            follow: None,
        },
        &no_lengths,
    )
    .expect("section");
    assert_eq!(m.sections.len(), 2);
    assert_eq!(m.sections[0].start, 0.0);
    assert_eq!(m.sections[1].next, SectionNext::Stop);
    // A second clip on an upper-body track (new track).
    m.apply_edit(
        &MontageEdit::AddSegment {
            slot: "upper".into(),
            index: None,
            clip: "tagged".into(),
            start: 0.0,
            end: None,
            rate: 1.0,
            loops: 2,
        },
        &no_lengths,
    )
    .expect("track");
    assert_eq!(m.tracks.len(), 2);
    // Notify linked to the main segment, scaled with its length.
    m.apply_edit(
        &MontageEdit::AddNotify {
            name: "hit".into(),
            time: 1.0,
            duration: 0.0,
            payload: None,
            branch: false,
            follow: Some(FollowEdit {
                track: 0,
                segment: 0,
                mode: FollowModeEdit::ScaleWithLength,
            }),
        },
        &no_lengths,
    )
    .expect("notify");
    // Half the clip at rate 1: the notify keeps its fraction (1.0 of 2.0 s → 0.5 of 1.0 s).
    let report = m.edit_segment_with(0, 0, 0.0, Some(1.0), &lengths).expect("trim");
    assert_eq!(m.notifies[0].time, 0.5);
    assert_eq!(report.moved[0].kind, EditedItem::Notify);
    // A bad link target is refused and nothing changes.
    let before = m.clone();
    let err = m
        .apply_edit(
            &MontageEdit::LinkSections {
                from: "main".into(),
                to: Some("nope".into()),
                stop: false,
            },
            &no_lengths,
        )
        .expect_err("unknown section");
    assert_eq!(err.code(), "montage_bad_edit");
    assert_eq!(m, before);
    // Removing the only main-track segment is refused.
    let err = m
        .apply_edit(&MontageEdit::RemoveSegment { track: 0, segment: 0 }, &no_lengths)
        .expect_err("main track");
    assert_eq!(err.code(), "montage_bad_edit");
    // Removing the upper track's only segment removes the track.
    m.apply_edit(&MontageEdit::RemoveSegment { track: 1, segment: 0 }, &no_lengths)
        .expect("remove");
    assert_eq!(m.tracks.len(), 1);
    // The result still round-trips through the file format.
    let back = Montage::from_ron_str(&m.to_ron_string().expect("write")).expect("read");
    assert_eq!(back, m);
}

#[test]
fn edit_refuses_a_bad_warp_and_rate() {
    let mut m = Montage::single("w", "full_body", "strike");
    let none = |_: &str| None;
    let bad_warp = MontageEdit::AddWarp {
        target: "ledge".into(),
        start: 1.0,
        end: 0.5,
        translation: true,
        vertical: false,
        rotation: false,
        mode: None,
        rotation_kind: None,
        rotation_fraction: 1.0,
        anchor_bone: None,
        anchor_offset: None,
    };
    assert_eq!(
        m.apply_edit(&bad_warp, &none).map_err(|e| e.code()),
        Err("montage_invalid")
    );
    let bad_rate = MontageEdit::MoveSegment {
        track: 0,
        segment: 0,
        start: None,
        end: None,
        to_clip_end: false,
        to: None,
        rate: Some(0.0),
        loops: None,
    };
    assert_eq!(
        m.apply_edit(&bad_rate, &none).map_err(|e| e.code()),
        Err("montage_invalid")
    );
    assert!(m.warps.is_empty());
    assert_eq!(m.tracks[0].segments[0].rate, 1.0);
}

// ---------------------------------------------------------------- reverse

#[test]
fn reverse_plays_back_to_zero_with_inverse_root_motion_and_reverse_events() {
    let mut forward = Rig::new();
    forward.player.play(Arc::new(marked()), PlayOptions::default());
    forward.ended_after(0.05, 100).expect("forward ends");

    let mut rig = Rig::new();
    let id = rig.player.play(
        Arc::new(marked()),
        PlayOptions {
            reverse: true,
            ..PlayOptions::default()
        },
    );
    rig.step(0.0);
    let status = rig.player.status(id).expect("playing");
    assert!(status.reverse);
    assert!(
        (status.time - 2.0).abs() < 1e-6,
        "starts at the end: {}",
        status.time
    );
    let frames = rig.ended_after(0.05, 100).expect("reverse ends");
    assert!((40..=41).contains(&frames), "2 s at 0.05 s frames, got {frames}");
    // The rig walks back exactly the forward path.
    assert!((forward.total.z + 2.0).abs() < 1e-3, "{}", forward.total);
    assert!((rig.total.z - 2.0).abs() < 1e-3, "{}", rig.total);
    let kinds = rig.kinds();
    let order: Vec<&str> = kinds
        .iter()
        .map(String::as_str)
        .filter(|k| !k.starts_with("blend_out"))
        .collect();
    assert_eq!(
        order,
        [
            "started",
            "notify late",
            "begin win",
            "section b->a",
            "end win",
            "notify early",
            "ended false"
        ],
        "{kinds:?}"
    );
}

#[test]
fn reverse_from_a_section_and_toggle_mid_play() {
    let mut rig = Rig::new();
    let id = rig.player.play(
        Arc::new(marked()),
        PlayOptions {
            reverse: true,
            section: Some("a".into()),
            start_time: 0.25,
            ..PlayOptions::default()
        },
    );
    rig.step(0.0);
    let status = rig.player.status(id).expect("playing");
    assert!(
        (status.time - 0.75).abs() < 1e-6,
        "end of a (1.0) minus 0.25: {}",
        status.time
    );
    assert_eq!(status.section, "a");
    rig.step(0.25);
    assert!((rig.player.status(id).expect("playing").time - 0.5).abs() < 1e-5);
    // Turn around: forward again from 0.5.
    assert!(rig.player.set_reverse(Some(id), false));
    rig.step(0.25);
    let status = rig.player.status(id).expect("playing");
    assert!(!status.reverse);
    assert!((status.time - 0.75).abs() < 1e-5, "{}", status.time);
}

#[test]
fn reverse_clip_notifies_fire_backward() {
    let mut rig = Rig::new();
    let mut m = Montage::single("tag", "full_body", "tagged");
    m.blend_in.time = 0.0;
    rig.player.play(
        Arc::new(m),
        PlayOptions {
            reverse: true,
            ..PlayOptions::default()
        },
    );
    rig.step(0.0);
    for _ in 0..4 {
        rig.step(0.2);
    }
    assert_eq!(
        rig.notifies.iter().map(|n| n.name.as_str()).collect::<Vec<_>>(),
        ["whoosh"]
    );
}

#[test]
fn try_play_checks_reverse_start_and_duration() {
    let mut player = MontagePlayer::new(vec![SlotDef::full_body("full_body", "body")], library());
    let m = Arc::new(marked());
    let past = PlayOptions {
        reverse: true,
        section: Some("b".into()),
        start_time: 1.5,
        ..PlayOptions::default()
    };
    assert_eq!(player.try_play(m.clone(), past), Err(PlayError::StartPastEnd));
    for duration in [0.0, -1.0, f32::NAN] {
        let options = PlayOptions {
            duration: Some(duration),
            ..PlayOptions::default()
        };
        assert_eq!(player.try_play(m.clone(), options), Err(PlayError::BadRate));
    }
}

#[test]
fn reverse_branch_jump_lands_at_the_target_sections_end() {
    let mut m = marked();
    m.notifies = vec![MontageNotify {
        branch: true,
        ..notify("cancel", 1.5, 0.0)
    }];
    let mut rig = Rig::new();
    let id = rig.player.play(
        Arc::new(m),
        PlayOptions {
            reverse: true,
            ..PlayOptions::default()
        },
    );
    rig.step(0.0);
    assert!(
        rig.player
            .on_branch(Some(id), "cancel", BranchAction::JumpTo("a".into()))
    );
    // 0.5 s back to the branch at 1.5, the jump to a's end (1.0), then 0.1 s more.
    rig.step(0.6);
    let status = rig.player.status(id).expect("playing");
    assert!((status.time - 0.9).abs() < 1e-5, "{}", status.time);
    assert_eq!(status.section, "a");
    let kinds = rig.kinds();
    assert!(kinds.contains(&"branch cancel".to_string()), "{kinds:?}");
    assert!(kinds.contains(&"section b->a".to_string()), "{kinds:?}");
}

#[test]
fn reverse_branch_stop_ends_the_montage() {
    let mut m = marked();
    m.notifies = vec![MontageNotify {
        branch: true,
        ..notify("cancel", 1.5, 0.0)
    }];
    let mut rig = Rig::new();
    let id = rig.player.play(
        Arc::new(m),
        PlayOptions {
            reverse: true,
            ..PlayOptions::default()
        },
    );
    rig.step(0.0);
    assert!(rig.player.on_branch(Some(id), "cancel", BranchAction::Stop));
    assert!(rig.ended_after(0.25, 8).is_some(), "{:?}", rig.kinds());
    assert!(
        rig.kinds().iter().all(|k| k != "section b->a"),
        "{:?}",
        rig.kinds()
    );
}

#[test]
fn reverse_crosses_sections_in_timeline_order_and_ignores_forward_links() {
    let mut m = Montage::single("linked", "full_body", "strike");
    m.blend_in.time = 0.0;
    m.blend_out.time = 0.0;
    m.sections = vec![
        MontageSection {
            next: SectionNext::Jump("c".into()),
            ..section("a", 0.0)
        },
        MontageSection {
            next: SectionNext::Stop,
            ..section("b", 0.5)
        },
        section("c", 1.0),
    ];
    let mut rig = Rig::new();
    rig.player.play(
        Arc::new(m),
        PlayOptions {
            reverse: true,
            ..PlayOptions::default()
        },
    );
    rig.ended_after(0.05, 100).expect("ends at 0");
    let kinds = rig.kinds();
    let sections: Vec<&str> = kinds
        .iter()
        .map(String::as_str)
        .filter(|k| k.starts_with("section"))
        .collect();
    assert_eq!(sections, ["section c->b", "section b->a"], "{kinds:?}");
}

// ---------------------------------------------------------------- time-stretch

#[test]
fn duration_stretches_the_montage_to_the_wall_time() {
    for (duration, dt) in [(4.0_f32, 1.0 / 60.0_f32), (1.0, 1.0 / 30.0)] {
        let mut rig = Rig::new();
        rig.player.play(
            Arc::new(marked()),
            PlayOptions {
                duration: Some(duration),
                ..PlayOptions::default()
            },
        );
        let frames = rig.ended_after(dt, 1000).expect("ends");
        let wall = frames as f32 * dt;
        assert!(
            (wall - duration).abs() <= dt + 1e-4,
            "{duration} s asked, ended after {wall} s"
        );
    }
}

#[test]
fn duration_accounts_for_the_rate_curve_and_stretch_to_rescales_the_rest() {
    let mut m = marked();
    // Double speed over the second second: 1 s + 0.5 s = 1.5 s at rate 1.
    m.rate_curve = Some(RateCurve {
        keys: vec![(0.0, 1.0), (1.0, 2.0)],
        interpolation: RateInterpolation::Step,
    });
    let mut rig = Rig::new();
    rig.player.play(
        Arc::new(m.clone()),
        PlayOptions {
            duration: Some(3.0),
            ..PlayOptions::default()
        },
    );
    let dt = 1.0 / 60.0;
    let frames = rig.ended_after(dt, 1000).expect("ends");
    assert!((frames as f32 * dt - 3.0).abs() <= dt + 1e-3, "{frames}");

    // Mid-play: after 0.5 s at rate 1, stretch the remaining 1.5 montage seconds (1.0 s of wall
    // time at rate 1 with the curve) to 2 s.
    let mut rig = Rig::new();
    let id = rig.player.play(Arc::new(m), PlayOptions::default());
    for _ in 0..30 {
        rig.step(dt);
    }
    assert!(rig.player.stretch_to(Some(id), 2.0));
    assert!(!rig.player.stretch_to(Some(id), 0.0));
    let frames = rig.ended_after(dt, 1000).expect("ends");
    assert!((frames as f32 * dt - 2.0).abs() <= dt + 1e-3, "{frames}");
}

#[test]
fn duration_with_reverse_stretches_the_way_back() {
    let mut rig = Rig::new();
    rig.player.play(
        Arc::new(marked()),
        PlayOptions {
            reverse: true,
            duration: Some(1.0),
            ..PlayOptions::default()
        },
    );
    let frames = rig.ended_after(0.05, 100).expect("ends");
    assert!((20..=21).contains(&frames), "{frames}");
}

// ---------------------------------------------------------------- M9 Slot node

fn clip_node(clip: &str) -> AnimNode {
    AnimNode::Clip(ClipNode {
        clip: clip.into(),
        looping: true,
        speed: 1.0,
        sync_group: None,
    })
}

/// A rig in an app, with a graph `root`, a montage player and `montage` playing.
fn app_rig(root: AnimNode, montage: Montage, blend_in: f32) -> (App, Entity) {
    let mut app = App::new();
    app.add_plugins((regia_app::MinimalPlugins, AnimationPlugin));
    app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
        Duration::from_secs_f32(1.0 / 32.0),
    ));
    let skeleton = Arc::new(skeleton());
    let rig = app.world_mut().spawn(Transform::default()).id();
    let binding = spawn_skeleton(app.world_mut(), skeleton);
    let mut clips = ClipLibrary::new();
    clips.insert(AnimationClip::empty("idle", 1.0));
    clips.insert(hold_arm("arm_high", 5.0));
    clips.insert(hold_root("root_up", 7.0));
    let graph = AnimGraph {
        parameters: vec![ParamDecl("W".into(), ParamValue::Float(1.0))],
        root,
    };
    graph.validate().expect("graph");
    let mut player = MontagePlayer::new(
        vec![
            SlotDef::full_body("full_body", "body"),
            SlotDef::masked("upper", "upper", &["arm"]),
        ],
        library(),
    );
    let mut montage = montage;
    montage.blend_in.time = blend_in;
    player.play(Arc::new(montage), PlayOptions::default());
    // `Animator::graph` starts with no parameters: the host applies the declared defaults.
    let parameters = graph.default_parameters();
    let mut animator = Animator::graph(graph, clips);
    animator.parameters = parameters;
    app.world_mut()
        .entity_mut(rig)
        .insert((binding, animator, player));
    (app, rig)
}

fn arm_y(app: &App, rig: Entity) -> f32 {
    let animator = app.world().get::<Animator>(rig).expect("animator");
    let pose: &Pose = animator.evaluated_pose();
    pose.translations[1].y
}

/// The layer that pins the arm at y 5, over `base`.
fn arm_layer_over(base: AnimNode) -> AnimNode {
    AnimNode::Layer(LayerNode {
        base: Box::new(base),
        layers: vec![LayerEntry {
            graph: Box::new(clip_node("arm_high")),
            mask: JointMaskDef {
                joints: vec!["arm".into()],
                include_children: true,
            },
            mode: LayerMode::Override,
            weight_param: "W".into(),
            weight_inertialize: 0.0,
        }],
    })
}

#[test]
fn slot_node_lets_nodes_above_it_see_the_montage() {
    let strike = || Montage::single("strike", "full_body", "strike");
    // Without a Slot node the montage applies after the graph: its arm wins over the layer.
    let (mut app, rig) = app_rig(arm_layer_over(clip_node("idle")), strike(), 0.0);
    for _ in 0..8 {
        app.update();
    }
    let post_graph = arm_y(&app, rig);
    assert!(post_graph < 3.0 && post_graph > 1.0, "montage arm: {post_graph}");
    // With the Slot node under the layer, the layer (above the montage) wins.
    let slot = AnimNode::Slot(SlotNode {
        name: "full_body".into(),
        source: Box::new(clip_node("idle")),
        eval_idle_source: true,
    });
    let (mut app, rig) = app_rig(arm_layer_over(slot), strike(), 0.0);
    for _ in 0..8 {
        app.update();
    }
    assert!(
        (arm_y(&app, rig) - 5.0).abs() < 1e-5,
        "layer arm: {}",
        arm_y(&app, rig)
    );
}

#[test]
fn slot_node_blends_once_and_matches_the_post_graph_stage() {
    // Half-way through a 1 s blend-in the montage weight is partial: a double blend (in the
    // node and again after the graph) would pull the arm further toward the montage.
    let strike = || Montage::single("strike", "full_body", "strike");
    let (mut plain, plain_rig) = app_rig(clip_node("idle"), strike(), 1.0);
    let slot = AnimNode::Slot(SlotNode {
        name: "full_body".into(),
        source: Box::new(clip_node("idle")),
        eval_idle_source: true,
    });
    let (mut slotted, slot_rig) = app_rig(slot, strike(), 1.0);
    for frame in 0..40 {
        plain.update();
        slotted.update();
        let (a, b) = (arm_y(&plain, plain_rig), arm_y(&slotted, slot_rig));
        assert!(
            (a - b).abs() < 1e-5,
            "frame {frame}: post-graph {a}, slot node {b}"
        );
    }
    // The montage's root motion still reaches the rig through the slot path.
    let (t, _) = slotted
        .world()
        .get::<Animator>(slot_rig)
        .expect("animator")
        .last_root_motion();
    let (u, _) = plain
        .world()
        .get::<Animator>(plain_rig)
        .expect("animator")
        .last_root_motion();
    assert!((t - u).length() < 1e-6, "{t} vs {u}");
}

#[test]
fn slot_node_for_another_slot_leaves_the_post_graph_stage_in_charge() {
    // The node plays `upper`; the montage plays `full_body`, so the post-graph stage blends it.
    let slot = AnimNode::Slot(SlotNode {
        name: "upper".into(),
        source: Box::new(clip_node("idle")),
        eval_idle_source: false,
    });
    let (mut app, rig) = app_rig(
        arm_layer_over(slot),
        Montage::single("strike", "full_body", "strike"),
        0.0,
    );
    for _ in 0..8 {
        app.update();
    }
    let y = arm_y(&app, rig);
    assert!(y < 3.0 && y > 1.0, "post-graph montage arm: {y}");
}

#[test]
fn slot_node_in_an_inactive_state_leaves_the_montage_to_the_post_graph_stage() {
    // The Slot node sits in state `b`, the machine stays in `a`: no node consumes the slot, so
    // the post-graph stage still plays it (and only it).
    let text = r#"(format_version: 3, parameters: [("W", Float(1.0))], root: StateMachine((
        initial: "a",
        states: [
            ("a", Clip((clip: "idle", looping: true, speed: 1.0, sync_group: None))),
            ("b", Slot((name: "full_body",
                source: Clip((clip: "idle", looping: true, speed: 1.0, sync_group: None))))),
        ],
        transitions: [],
    )))"#;
    let root = AnimGraph::from_ron_str(text).expect("graph").root;
    let (mut app, rig) = app_rig(root, Montage::single("strike", "full_body", "strike"), 0.0);
    for _ in 0..8 {
        app.update();
    }
    assert!(
        (arm_y(&app, rig) - 1.25).abs() < 1e-4,
        "montage arm: {}",
        arm_y(&app, rig)
    );
}

#[test]
fn slot_node_crossfade_between_two_montages_matches_the_post_graph_stage() {
    let strike = || Montage::single("strike", "full_body", "strike");
    let (mut plain, plain_rig) = app_rig(clip_node("idle"), strike(), 0.0);
    let slot = AnimNode::Slot(SlotNode {
        name: "full_body".into(),
        source: Box::new(clip_node("idle")),
        eval_idle_source: true,
    });
    let (mut slotted, slot_rig) = app_rig(slot, strike(), 0.0);
    for frame in 0..48 {
        // Half a second in, a second montage of the same group interrupts with a 0.5 s blend.
        if frame == 16 {
            for (app, rig) in [(&mut plain, plain_rig), (&mut slotted, slot_rig)] {
                let mut next = Montage::single("tagged", "full_body", "tagged");
                next.blend_in.time = 0.5;
                let mut player = app.world_mut().get_mut::<MontagePlayer>(rig).expect("player");
                player.play(Arc::new(next), PlayOptions::default());
            }
        }
        plain.update();
        slotted.update();
        let (a, b) = (arm_y(&plain, plain_rig), arm_y(&slotted, slot_rig));
        assert!(
            (a - b).abs() < 1e-5,
            "frame {frame}: post-graph {a}, slot node {b}"
        );
    }
}

fn root_y(app: &App, rig: Entity) -> f32 {
    let animator = app.world().get::<Animator>(rig).expect("animator");
    let pose: &Pose = animator.evaluated_pose();
    pose.translations[0].y
}

#[test]
fn eval_idle_source_false_skips_the_source_only_under_a_full_body_slot() {
    let slot = |name: &str| {
        AnimNode::Slot(SlotNode {
            name: name.into(),
            source: Box::new(clip_node("root_up")),
            eval_idle_source: false,
        })
    };
    // Full body at weight 1 hides the source: the pose comes from the montage, not `root_up`.
    let (mut app, rig) = app_rig(
        slot("full_body"),
        Montage::single("strike", "full_body", "strike"),
        0.0,
    );
    for _ in 0..4 {
        app.update();
    }
    assert!(
        root_y(&app, rig).abs() < 1e-5,
        "montage root: {}",
        root_y(&app, rig)
    );
    // A masked slot covers only the arm: the source still drives the root.
    let (mut app, rig) = app_rig(slot("upper"), Montage::single("strike", "upper", "strike"), 0.0);
    for _ in 0..4 {
        app.update();
    }
    assert!(
        (root_y(&app, rig) - 7.0).abs() < 1e-5,
        "source root: {}",
        root_y(&app, rig)
    );
    let y = arm_y(&app, rig);
    assert!(y < 3.0 && y > 1.0, "montage arm: {y}");
}

#[test]
fn slot_nodes_rename_and_check_against_the_rig_slots() {
    let mut graph = AnimGraph {
        parameters: vec![],
        root: arm_layer_over(AnimNode::Slot(SlotNode {
            name: "upper".into(),
            source: Box::new(AnimNode::Slot(SlotNode {
                name: "upper".into(),
                source: Box::new(clip_node("idle")),
                eval_idle_source: true,
            })),
            eval_idle_source: true,
        })),
    };
    let defs = [SlotDef::full_body("full_body", "body")];
    assert_eq!(graph.check_slot_defs(&defs).len(), 1);
    assert!(graph.check_slot_defs(&defs)[0].starts_with("animgraph_unknown_slot"));
    assert!(
        graph.check_slot_defs(&[]).is_empty(),
        "no slot list: every slot plays full body"
    );
    assert_eq!(graph.rename_slot("upper", "full_body"), 2);
    assert_eq!(graph.slot_names(), ["full_body"]);
    assert!(graph.check_slot_defs(&defs).is_empty());
    assert_eq!(graph.rename_slot("upper", "x"), 0);
}

#[test]
fn animgraph_v3_round_trips_the_slot_node_and_v2_still_loads() {
    let graph = AnimGraph {
        parameters: vec![],
        root: AnimNode::Slot(SlotNode {
            name: "upper".into(),
            source: Box::new(clip_node("idle")),
            eval_idle_source: false,
        }),
    };
    let text = graph.to_ron_string().expect("write");
    assert!(
        text.contains(&format!("format_version: {ANIMGRAPH_FORMAT_VERSION}")),
        "{text}"
    );
    // The Slot node arrived in v3; v4 (ADR-8475) only adds the IK nodes.
    const { assert!(ANIMGRAPH_FORMAT_VERSION >= 3) };
    let back = AnimGraph::from_ron_str(&text).expect("read");
    assert_eq!(back, graph);
    assert_eq!(back.slot_names(), ["upper"]);
    assert!(back.has_slot_nodes());
    // `eval_idle_source` defaults to true.
    let short = r#"(format_version: 3, parameters: [], root: Slot((name: "full", source: Clip((clip: "idle", looping: true, speed: 1.0, sync_group: None)))))"#;
    let AnimNode::Slot(node) = AnimGraph::from_ron_str(short).expect("short").root else {
        panic!("slot root");
    };
    assert!(node.eval_idle_source);
    // A v2 file (no Slot node) loads through the 2 → 3 migrator.
    let v2 = r#"(format_version: 2, parameters: [], root: Clip((clip: "idle", looping: true, speed: 1.0, sync_group: None)))"#;
    let loaded = AnimGraph::from_ron_str(v2).expect("v2");
    assert!(!loaded.has_slot_nodes());
    // An empty slot name is refused.
    let empty = short.replace("\"full\"", "\"\"");
    assert!(AnimGraph::from_ron_str(&empty).is_err());
    // A slot the skeleton's table lacks is a load warning.
    let table =
        SkeletonSlots::from_ron_str(r#"(format_version: 1, slots: [(name: "full", group: "default")])"#)
            .expect("table");
    assert!(back.check_slots(&table)[0].starts_with("animgraph_unknown_slot"));
    assert!(loaded.check_slots(&table).is_empty());
}
