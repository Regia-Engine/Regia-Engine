// SPDX-License-Identifier: MPL-2.0
//! The graph's locomotion warp nodes (`.animgraph.ron` v4, ADR-8540): `StrideWarp` (`Rate` scales
//! the inner clock, `Pose` scales the stride with leg IK) and `OrientationWarp` (the lower body
//! turns toward the movement heading, the spine turns back), on the demo humanoid's walk and run
//! (`demo_locomotion`: planted feet, root motion on the root joint).
//!
//! The skating measure is foot planting's acceptance (P-10): per stance, the largest horizontal
//! distance of the posed ankle in world space from where it touched down; the character moves at
//! the gameplay ground speed, never by the clip's root motion.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;

use regia_animation::Skeleton;
use regia_animation::blendspace::{BlendSample1D, BlendSpace1D};
use regia_animation::debug::{NodeDebugState, graph_debug};
use regia_animation::demo_rig::{DEMO_LEGS, DemoGait, DemoRig, demo_humanoid, demo_locomotion};
use regia_animation::eval::{ClipLibrary, EvalContext, EvalError, EvalReuse, GraphInstance, evaluate_graph};
use regia_animation::graph::{
    ANIMGRAPH_FORMAT_VERSION, AnimGraph, AnimGraphError, AnimNode, BlendSpace1DNode, ClipNode,
    OrientationWarpNode, StrideWarpNode,
};
use regia_animation::ik::joint_position;
use regia_animation::motion_matching::warp::{StrideMode, StrideWarpSettings};
use regia_animation::params::{ParamDecl, ParamValue, ParameterSet};
use regia_animation::pose::Pose;
use regia_animation::warp_nodes::movement_heading;
use regia_core::math::{Quat, Vec3};
use regia_ecs::prelude::Entity;

const DT: f32 = 1.0 / 60.0;
/// The demo walk's authored speed, m/s (`DemoGait::standard`).
const WALK_SPEED: f32 = 1.3;
/// Gameplay speed over the clip speed.
const OVERSPEED: f32 = 1.3;

fn rig() -> (Skeleton, ClipLibrary) {
    let skeleton = demo_humanoid(&DemoRig::default()).expect("demo rig");
    let mut clips = ClipLibrary::new();
    for clip in demo_locomotion(&skeleton, &DemoGait::standard()) {
        clips.insert(clip);
    }
    (skeleton, clips)
}

/// Walk at 1.3 m/s and run at 3.2 m/s on one synced clock, by the `Speed` parameter.
fn locomotion() -> Box<AnimNode> {
    Box::new(AnimNode::BlendSpace1D(BlendSpace1DNode {
        space: BlendSpace1D {
            param: "Speed".into(),
            samples: vec![
                BlendSample1D {
                    value: WALK_SPEED,
                    clip: "walk".into(),
                },
                BlendSample1D {
                    value: 3.2,
                    clip: "run".into(),
                },
            ],
        },
        sync_group: Some("gait".into()),
    }))
}

fn parameters() -> Vec<ParamDecl> {
    vec![
        ParamDecl("Speed".into(), ParamValue::Float(WALK_SPEED)),
        ParamDecl("GroundSpeed".into(), ParamValue::Float(WALK_SPEED)),
        ParamDecl("Heading".into(), ParamValue::Float(0.0)),
    ]
}

fn legs() -> Vec<[String; 3]> {
    vec![
        ["left_thigh".into(), "left_calf".into(), "left_foot".into()],
        ["right_thigh".into(), "right_calf".into(), "right_foot".into()],
    ]
}

fn stride_settings(mode: StrideMode) -> StrideWarpSettings {
    StrideWarpSettings {
        ratio_min: 0.5,
        ratio_max: 2.0,
        mode,
        ..StrideWarpSettings::graph_default()
    }
}

fn stride_node(inner: Box<AnimNode>, mode: StrideMode) -> AnimNode {
    AnimNode::StrideWarp(StrideWarpNode {
        inner,
        ground_speed_param: "GroundSpeed".into(),
        settings: stride_settings(mode),
        legs: legs(),
        pelvis: "hips".into(),
        stride_length: 0.5,
    })
}

fn orientation_node(inner: Box<AnimNode>, max_delta: f32) -> AnimNode {
    AnimNode::OrientationWarp(OrientationWarpNode {
        inner,
        heading_param: "Heading".into(),
        pelvis: "hips".into(),
        spine: vec![("spine".into(), 0.5), ("chest".into(), 0.5)],
        strength: 1.0,
        max_delta,
    })
}

fn graph(root: AnimNode) -> AnimGraph {
    AnimGraph {
        parameters: parameters(),
        root,
    }
}

/// One rig evaluating one graph frame by frame, like the animator (reuse and scratch kept).
struct Runner<'a> {
    skeleton: &'a Skeleton,
    clips: &'a ClipLibrary,
    graph: &'a AnimGraph,
    instance: GraphInstance,
    reuse: EvalReuse,
    scratch: Vec<Pose>,
    sockets: HashMap<(Entity, String), Vec3>,
    pose: Pose,
    root_motion: Vec3,
}

impl<'a> Runner<'a> {
    fn new(skeleton: &'a Skeleton, clips: &'a ClipLibrary, graph: &'a AnimGraph) -> Self {
        Runner {
            skeleton,
            clips,
            graph,
            instance: GraphInstance::from_node(&graph.root),
            reuse: EvalReuse::default(),
            scratch: Vec::new(),
            sockets: HashMap::new(),
            pose: Pose::from_identity(skeleton.len()),
            root_motion: Vec3::ZERO,
        }
    }

    fn try_step(&mut self, params: &ParameterSet) -> Result<(), EvalError> {
        let mut ctx = EvalContext::new(
            self.skeleton,
            self.clips,
            params,
            DT,
            Entity::PLACEHOLDER,
            &self.sockets,
            std::mem::take(&mut self.scratch),
        )
        .with_reuse(std::mem::take(&mut self.reuse));
        let result = evaluate_graph(self.graph, &mut self.instance, &mut ctx, &mut self.pose);
        self.root_motion = ctx.root_motion.0;
        self.reuse = ctx.take_reuse();
        self.scratch = ctx.finish().scratch;
        result
    }

    fn step(&mut self, params: &ParameterSet) {
        self.try_step(params).expect("evaluate");
    }

    fn ankle(&self, leg: usize) -> Vec3 {
        joint_position(self.skeleton, &self.pose, DEMO_LEGS[leg][2] as u32)
    }

    fn rotation(&self, joint: &str) -> Quat {
        let index = self.skeleton.joint_index(joint).expect("joint") as usize;
        self.skeleton.globals(&self.pose, None)[index]
            .to_scale_rotation_translation()
            .1
    }

    /// The `(walk, run, shared)` clocks of the blend space under the root warp node.
    fn blend_clocks(&self) -> (f32, f32, f32) {
        let space = match &self.instance {
            GraphInstance::Warp { inner, .. } => inner.as_ref(),
            other => other,
        };
        let GraphInstance::BlendSpace1D(runtime) = space else {
            panic!("blend space instance");
        };
        (
            runtime.states[0].time,
            runtime.states[1].time,
            runtime.states[2].time,
        )
    }
}

fn horizontal(a: Vec3, b: Vec3) -> f32 {
    Vec3::new(a.x - b.x, 0.0, a.z - b.z).length()
}

/// Per-stance skating of a walk: the largest horizontal distance of a posed ankle (world space)
/// from where it touched down, and the stances seen.
#[derive(Debug, Default)]
struct Skate {
    slide: f32,
    stances: usize,
}

/// Walks `graph` for `seconds` with the character moving forward (−Z) at `ground_speed`, and
/// measures the planted ankles after a one-second settle. A frame is planted for a leg when
/// `planted_from`'s ankle (the unwarped pose for `Pose` mode, the graph's own for `Rate`) sits
/// at the authored ankle height.
fn skate(graph: &AnimGraph, ground_speed: f32, seconds: f32, own_contacts: bool) -> Skate {
    let (skeleton, clips) = rig();
    let control_graph = AnimGraph {
        parameters: parameters(),
        root: *locomotion(),
    };
    let mut warped = Runner::new(&skeleton, &clips, graph);
    let mut control = Runner::new(&skeleton, &clips, &control_graph);
    let mut params = graph.default_parameters();
    params.set_float("GroundSpeed", ground_speed);
    let ankle_height = joint_position(&skeleton, &skeleton.bind_pose_pose(), DEMO_LEGS[0][2] as u32).y;
    let mut out = Skate::default();
    let mut touchdown: [Option<Vec3>; 2] = [None; 2];
    let frames = (seconds / DT) as usize;
    let settle = (1.0 / DT) as usize;
    for frame in 0..frames {
        warped.step(&params);
        control.step(&params);
        let rig = Vec3::NEG_Z * (ground_speed * frame as f32 * DT);
        for (leg, touch) in touchdown.iter_mut().enumerate() {
            let contact = if own_contacts { &warped } else { &control };
            let planted = (contact.ankle(leg).y - ankle_height).abs() < 1.0e-3;
            if frame <= settle || !planted {
                *touch = None;
                continue;
            }
            let ankle = rig + warped.ankle(leg);
            let start = *touch.get_or_insert_with(|| {
                out.stances += 1;
                ankle
            });
            out.slide = out.slide.max(horizontal(ankle, start));
        }
    }
    out
}

/// W1 acceptance: a blend-space walk driven 1.3x faster than its clips skates by more than 5 cm
/// per stance without a warp node and by at most 1 cm with a `Pose` stride warp (and with a
/// `Rate` one).
#[test]
fn blend_space_walk_at_1_3x_ground_speed_does_not_skate_with_pose_warp() {
    let ground = WALK_SPEED * OVERSPEED;
    let control = skate(&graph(*locomotion()), ground, 5.0, false);
    println!(
        "warp control slide={:.4}m stances={}",
        control.slide, control.stances
    );
    assert!(control.stances >= 6, "{control:?}");
    assert!(control.slide > 0.05, "the control must skate: {control:?}");

    let pose = skate(
        &graph(stride_node(locomotion(), StrideMode::Pose)),
        ground,
        5.0,
        false,
    );
    println!("warp pose slide={:.4}m stances={}", pose.slide, pose.stances);
    assert!(pose.stances >= 6, "{pose:?}");
    assert!(pose.slide <= 0.01, "Pose warp: {pose:?}");

    let rate = skate(
        &graph(stride_node(locomotion(), StrideMode::Rate)),
        ground,
        5.0,
        true,
    );
    println!("warp rate slide={:.4}m stances={}", rate.slide, rate.stances);
    assert!(rate.stances >= 6, "{rate:?}");
    assert!(rate.slide <= 0.01, "Rate warp: {rate:?}");

    // A slower character shortens the stride the same way.
    let slow = skate(
        &graph(stride_node(locomotion(), StrideMode::Pose)),
        WALK_SPEED * 0.8,
        5.0,
        false,
    );
    println!("warp pose slow slide={:.4}m stances={}", slow.slide, slow.stances);
    assert!(slow.slide <= 0.01, "Pose warp at 0.8x: {slow:?}");
}

/// `Rate` plays the whole synced blend space on the scaled clock: walk and run keep one phase
/// every frame, and the shared phase advances by the ratio times the unwarped advance, so the
/// root motion matches the ground speed.
#[test]
fn rate_mode_keeps_sync_group_phase() {
    let (skeleton, clips) = rig();
    let warped_graph = graph(stride_node(locomotion(), StrideMode::Rate));
    let control_graph = graph(*locomotion());
    let mut params = warped_graph.default_parameters();
    params.set_float("Speed", 2.0);
    // The control's clip speed at this blend.
    let mut control = Runner::new(&skeleton, &clips, &control_graph);
    for _ in 0..10 {
        control.step(&params);
    }
    let clip_speed = Vec3::new(control.root_motion.x, 0.0, control.root_motion.z).length() / DT;
    assert!(clip_speed > 1.5 && clip_speed < 3.0, "clip speed {clip_speed}");
    let ground = clip_speed * 1.25;
    params.set_float("GroundSpeed", ground);

    let mut warped = Runner::new(&skeleton, &clips, &warped_graph);
    let mut control = Runner::new(&skeleton, &clips, &control_graph);
    let walk = clips.get("walk").expect("walk").duration;
    let run = clips.get("run").expect("run").duration;
    let (mut warped_phase, mut control_phase) = (0.0f32, 0.0f32);
    let mut previous = (0.0f32, 0.0f32);
    for frame in 0..240 {
        warped.step(&params);
        control.step(&params);
        let (w, r, shared) = warped.blend_clocks();
        assert!(
            (w / walk - r / run).abs() < 1e-4,
            "frame {frame}: walk phase {} vs run phase {}",
            w / walk,
            r / run
        );
        let control_shared = control.blend_clocks().2;
        if frame >= 120 {
            warped_phase += (shared - previous.0).rem_euclid(1.0);
            control_phase += (control_shared - previous.1).rem_euclid(1.0);
            let speed = Vec3::new(warped.root_motion.x, 0.0, warped.root_motion.z).length() / DT;
            assert!(
                (speed - ground).abs() <= ground * 1e-3,
                "frame {frame}: root motion {speed} m/s vs ground speed {ground}"
            );
        }
        previous = (shared, control_shared);
    }
    let ratio = warped_phase / control_phase;
    println!("warp rate phase ratio={ratio:.5}");
    assert!((ratio - 1.25).abs() < 1e-3, "phase advance ratio {ratio}");
    let debug = graph_debug(&warped_graph, &warped.instance, &params);
    let Some(NodeDebugState::StrideWarp { ratio, mode, .. }) = debug.node("root").map(|n| &n.state) else {
        panic!("stride warp debug state: {debug:?}");
    };
    assert_eq!(*mode, StrideMode::Rate);
    assert!((ratio - 1.25).abs() < 1e-3, "debug ratio {ratio}");
}

fn yaw(rotation: Quat) -> f32 {
    movement_heading(rotation * Vec3::NEG_Z)
}

fn angle_between(a: Quat, b: Quat) -> f32 {
    let dot = a.dot(b).abs().min(1.0);
    2.0 * dot.acos()
}

/// The lower body turns toward the heading, clipped to `max_delta`, while the chest keeps its
/// orientation; the debug snapshot reports the turn.
#[test]
fn orientation_warp_turns_the_lower_body_within_its_limit_and_keeps_the_chest_heading() {
    let (skeleton, clips) = rig();
    let walk = || {
        Box::new(AnimNode::Clip(ClipNode {
            clip: "walk".into(),
            looping: true,
            speed: 1.0,
            sync_group: None,
        }))
    };
    let warped_graph = graph(orientation_node(walk(), 0.5));
    let control_graph = graph(*walk());
    for (heading, expected) in [(0.3f32, 0.3f32), (0.9, 0.5), (-0.4, -0.4), (-2.0, -0.5)] {
        let mut warped = Runner::new(&skeleton, &clips, &warped_graph);
        let mut control = Runner::new(&skeleton, &clips, &control_graph);
        let mut params = warped_graph.default_parameters();
        params.set_float("Heading", heading);
        for frame in 0..90 {
            warped.step(&params);
            control.step(&params);
            let turn = yaw(warped.rotation("hips")) - yaw(control.rotation("hips"));
            assert!(
                (turn - expected).abs() < 1e-4,
                "heading {heading} frame {frame}: pelvis turned {turn}, expected {expected}"
            );
            let chest = angle_between(warped.rotation("chest"), control.rotation("chest"));
            assert!(
                chest < 1e-3,
                "heading {heading} frame {frame}: chest moved {chest} rad"
            );
            assert_eq!(
                warped.root_motion, control.root_motion,
                "root motion is not changed"
            );
        }
        let debug = graph_debug(&warped_graph, &warped.instance, &params);
        let Some(NodeDebugState::OrientationWarp { delta, clip_heading }) =
            debug.node("root").map(|n| &n.state)
        else {
            panic!("orientation warp debug state: {debug:?}");
        };
        assert!((delta - expected).abs() < 1e-6, "debug delta {delta}");
        assert!(
            clip_heading.abs() < 1e-5,
            "the walk moves forward: {clip_heading}"
        );
    }
}

/// An inner graph without root motion (an in-place idle) leaves the pose and the clock alone.
#[test]
fn warp_nodes_without_root_motion_pass_the_pose() {
    let (skeleton, clips) = rig();
    let idle = || {
        Box::new(AnimNode::Clip(ClipNode {
            clip: "idle".into(),
            looping: true,
            speed: 1.0,
            sync_group: None,
        }))
    };
    let control_graph = graph(*idle());
    for root in [
        stride_node(idle(), StrideMode::Pose),
        stride_node(idle(), StrideMode::Rate),
        orientation_node(idle(), 0.5),
    ] {
        let warped_graph = graph(root);
        let mut warped = Runner::new(&skeleton, &clips, &warped_graph);
        let mut control = Runner::new(&skeleton, &clips, &control_graph);
        let mut params = warped_graph.default_parameters();
        params.set_float("GroundSpeed", 2.0);
        params.set_float("Heading", 0.7);
        for _ in 0..60 {
            warped.step(&params);
            control.step(&params);
            assert_eq!(warped.pose, control.pose);
        }
    }
}

#[test]
fn unknown_warp_joint_is_a_missing_joint_error() {
    let (skeleton, clips) = rig();
    let mut node = stride_node(locomotion(), StrideMode::Pose);
    if let AnimNode::StrideWarp(n) = &mut node {
        n.pelvis = "no_such_hips".into();
    }
    let g = graph(node);
    let mut runner = Runner::new(&skeleton, &clips, &g);
    let mut params = g.default_parameters();
    params.set_float("GroundSpeed", 2.0);
    let mut error = None;
    for _ in 0..10 {
        if let Err(e) = runner.try_step(&params) {
            error = Some(e);
            break;
        }
    }
    assert!(
        matches!(error, Some(EvalError::MissingJoint(ref name)) if name == "no_such_hips"),
        "{error:?}"
    );
}

/// Replays are bit-identical: two runners on the same inputs give the same poses.
#[test]
fn warp_nodes_replay_bit_identically() {
    let (skeleton, clips) = rig();
    let g = graph(orientation_node(
        Box::new(stride_node(locomotion(), StrideMode::Pose)),
        0.6,
    ));
    let mut a = Runner::new(&skeleton, &clips, &g);
    let mut b = Runner::new(&skeleton, &clips, &g);
    let mut params = g.default_parameters();
    for frame in 0..180 {
        params.set_float("GroundSpeed", 1.0 + (frame % 50) as f32 * 0.03);
        params.set_float("Heading", ((frame % 70) as f32 - 35.0) * 0.02);
        params.set_float("Speed", 1.3 + (frame % 90) as f32 * 0.01);
        a.step(&params);
        b.step(&params);
        assert_eq!(a.pose, b.pose, "frame {frame}");
        for q in &a.pose.rotations {
            assert!(q.is_finite() && (q.length() - 1.0).abs() < 1e-4, "{q:?}");
        }
    }
}

// ---- format (animgraph v4, the rules of the ChainIK/BodyAim batch) ----

fn both_nodes() -> AnimGraph {
    graph(orientation_node(
        Box::new(stride_node(locomotion(), StrideMode::Pose)),
        0.6,
    ))
}

#[test]
fn warp_nodes_round_trip_at_v4() {
    let g = both_nodes();
    g.validate().expect("valid");
    let text = g.to_ron_string().expect("write");
    assert!(text.contains(&format!("format_version: {ANIMGRAPH_FORMAT_VERSION}")));
    assert!(text.contains("StrideWarp(") && text.contains("OrientationWarp("));
    assert_eq!(AnimGraph::from_ron_str(&text).expect("load"), g);
    // Omitted optional fields take their documented defaults.
    let short = r#"(
        format_version: 4,
        parameters: [("GroundSpeed", Float(1.3)), ("Heading", Float(0.0))],
        root: OrientationWarp((
            inner: StrideWarp((
                inner: Clip((clip: "walk", looping: true, speed: 1.0, sync_group: None)),
                ground_speed_param: "GroundSpeed",
                legs: [("left_thigh", "left_calf", "left_foot")],
                pelvis: "hips",
            )),
            heading_param: "Heading",
            pelvis: "hips",
        )),
    )"#;
    let loaded = AnimGraph::from_ron_str(short).expect("short form loads");
    let AnimNode::OrientationWarp(orient) = &loaded.root else {
        panic!("orientation root");
    };
    assert_eq!(
        (orient.strength, orient.max_delta),
        (1.0, std::f32::consts::FRAC_PI_2)
    );
    assert!(orient.spine.is_empty());
    let AnimNode::StrideWarp(stride) = orient.inner.as_ref() else {
        panic!("stride inner");
    };
    assert_eq!(stride.settings, StrideWarpSettings::graph_default());
    assert_eq!(stride.settings.mode, StrideMode::Pose);
    assert_eq!(stride.stride_length, 0.5);
}

#[test]
fn v5_graph_with_warp_nodes_is_refused() {
    let text = both_nodes().to_ron_string().expect("write");
    let future = text.replacen(
        &format!("format_version: {ANIMGRAPH_FORMAT_VERSION}"),
        &format!("format_version: {}", ANIMGRAPH_FORMAT_VERSION + 1),
        1,
    );
    match AnimGraph::from_ron_str(&future) {
        Err(AnimGraphError::UnknownVersion(v)) if v == ANIMGRAPH_FORMAT_VERSION + 1 => {}
        other => panic!("a newer file must be refused: {other:?}"),
    }
}

/// A v3 file (no warp node) loads unchanged as v4: the 3 -> 4 step stays the identity.
#[test]
fn v3_graph_loads_unchanged_with_warp_kinds_in_the_format() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates/regia_agent/tests/fixtures/format-corpus/animgraph/v3.ron");
    let text = std::fs::read_to_string(&path).expect("v3 corpus fixture");
    assert_eq!(regia_core::version::peek_ron_version(&text).ok(), Some(3));
    let graph = AnimGraph::from_ron_str(&text).expect("v3 loads");
    let written = graph.to_ron_string().expect("write");
    assert!(written.contains("format_version: 4"));
    assert_eq!(AnimGraph::from_ron_str(&written).expect("reload"), graph);
}

type Spoil = (&'static str, fn(&mut AnimNode));

#[test]
fn warp_node_validation_refuses_bad_values() {
    let cases: [Spoil; 11] = [
        ("undeclared ground speed", |n| {
            with_stride(n, |s| s.ground_speed_param = "Nope".into())
        }),
        ("strength", |n| with_stride(n, |s| s.settings.strength = 1.5)),
        ("ratio range", |n| with_stride(n, |s| s.settings.ratio_min = 1.2)),
        ("response", |n| with_stride(n, |s| s.settings.response = f32::NAN)),
        ("stride length", |n| with_stride(n, |s| s.stride_length = -0.1)),
        ("pose without legs", |n| with_stride(n, |s| s.legs.clear())),
        ("empty leg joint", |n| {
            with_stride(n, |s| s.legs[0][1] = String::new())
        }),
        ("undeclared heading", |n| {
            with_orient(n, |o| o.heading_param = "Nope".into())
        }),
        ("spine sum", |n| with_orient(n, |o| o.spine[0].1 = 0.9)),
        ("max delta", |n| with_orient(n, |o| o.max_delta = 4.0)),
        ("no pelvis", |n| with_orient(n, |o| o.pelvis = String::new())),
    ];
    for (label, spoil) in cases {
        let mut g = both_nodes();
        g.validate().expect("the unspoiled graph is valid");
        spoil(&mut g.root);
        assert!(g.validate().is_err(), "{label}: a bad value was accepted");
    }
    // Rate mode needs no legs.
    let mut g = both_nodes();
    with_stride(&mut g.root, |s| {
        s.settings.mode = StrideMode::Rate;
        s.legs.clear();
        s.pelvis.clear();
    });
    g.validate().expect("Rate needs no legs");
}

fn with_orient(node: &mut AnimNode, edit: impl FnOnce(&mut OrientationWarpNode)) {
    if let AnimNode::OrientationWarp(o) = node {
        edit(o);
    }
}

fn with_stride(node: &mut AnimNode, edit: impl FnOnce(&mut StrideWarpNode)) {
    if let AnimNode::OrientationWarp(o) = node
        && let AnimNode::StrideWarp(s) = o.inner.as_mut()
    {
        edit(s);
    }
}

#[test]
fn published_warp_example_loads_and_round_trips() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/formats/locomotion_warp.animgraph.ron");
    let text = std::fs::read_to_string(&path).expect("example");
    let graph = AnimGraph::from_ron_str(&text).expect("the v4 example loads");
    let AnimNode::OrientationWarp(orient) = &graph.root else {
        panic!("orientation warp root");
    };
    assert_eq!(orient.spine.len(), 2);
    let AnimNode::StrideWarp(stride) = orient.inner.as_ref() else {
        panic!("stride warp inner");
    };
    assert_eq!(stride.legs.len(), 2);
    let written = graph.to_ron_string().expect("write");
    assert_eq!(AnimGraph::from_ron_str(&written).expect("reload"), graph);
}
