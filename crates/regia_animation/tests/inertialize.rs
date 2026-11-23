// SPDX-License-Identifier: MPL-2.0
//! Inertialized transitions (ADR-203): pose and velocity continuity at the switch and on
//! interruption, against cross-fades and cuts, plus the debug surface and v1 compatibility.
//!
//! Continuity is measured the way it is defined: the velocity jump at the switch must vanish
//! as the frame time shrinks (it is O(dt) for an inertialized transition) while a cross-fade's
//! stays O(1) (≈ 2·offset/duration) and a cut's position jump stays O(1).

use std::collections::HashMap;

use regia_animation::clip::{JointTrack, TrackCurve};
use regia_animation::debug::{NodeDebugState, graph_debug};
use regia_animation::eval::evaluate_graph;
use regia_animation::graph::AnimNode;
use regia_animation::skeleton::LocalBind;
use regia_animation::{
    AnimGraph, AnimationClip, ClipLibrary, EvalContext, GraphInstance, ParameterSet, Pose, Skeleton,
    TransitionBlend,
};
use regia_core::math::{Quat, Vec3};
use regia_ecs::prelude::Entity;

const CLIP_LEN: f32 = 4.0;

fn skeleton() -> Skeleton {
    Skeleton::from_local_bind(
        vec!["root".into(), "child".into()],
        vec![None, Some(0)],
        vec![
            LocalBind::default(),
            LocalBind {
                translation: Vec3::new(0.0, 1.0, 0.0),
                ..LocalBind::default()
            },
        ],
    )
    .expect("valid skeleton")
}

/// A clip whose child joint turns about Z by `angle(t)` and whose root slides along X by
/// `slide(t)`, keyed densely (2 kHz) so linear key interpolation is smooth at test rates.
fn clip(name: &str, angle: impl Fn(f32) -> f32, slide: impl Fn(f32) -> f32) -> AnimationClip {
    let n = (CLIP_LEN * 2000.0) as usize;
    let times: Vec<f32> = (0..=n).map(|i| i as f32 / 2000.0).collect();
    let mut c = AnimationClip::empty(name, CLIP_LEN);
    c.tracks.push(JointTrack {
        joint: 0,
        translations: TrackCurve::Animated {
            times: times.clone(),
            values: times.iter().map(|&t| Vec3::new(slide(t), 0.0, 0.0)).collect(),
            exact_rotation: false,
        },
        rotations: TrackCurve::Static(Quat::IDENTITY),
        scales: TrackCurve::Static(Vec3::ONE),
    });
    c.tracks.push(JointTrack {
        joint: 1,
        translations: TrackCurve::Static(Vec3::new(0.0, 1.0, 0.0)),
        rotations: TrackCurve::Animated {
            times: times.clone(),
            values: times.iter().map(|&t| Quat::from_rotation_z(angle(t))).collect(),
            exact_rotation: true,
        },
        scales: TrackCurve::Static(Vec3::ONE),
    });
    c
}

/// A → B on `Go`, B → A on `!Go`, both with `blend` and `duration`.
fn graph(blend: &str, duration: f32) -> AnimGraph {
    let text = format!(
        r#"(format_version: 2, parameters: [("Go", Bool(false))],
        root: StateMachine((initial: "A",
            states: [("A", Clip((clip: "A", looping: true, speed: 1.0, sync_group: None))),
                     ("B", Clip((clip: "B", looping: true, speed: 1.0, sync_group: None)))],
            transitions: [
                (from: Some("A"), to: "B", conditions: [Bool(param: "Go", value: true)],
                 duration: {duration}, curve: Linear, interruptible: true, blend: {blend}),
                (from: Some("B"), to: "A", conditions: [Bool(param: "Go", value: false)],
                 duration: {duration}, curve: Linear, interruptible: true, blend: {blend}),
            ])))"#
    );
    AnimGraph::from_ron_str(&text).expect("test graph parses")
}

/// Child angle about Z (radians) and root X of a pose.
fn channels(pose: &Pose) -> (f32, f32) {
    let q = pose.rotations[1];
    (2.0 * q.z.atan2(q.w), pose.translations[0].x)
}

struct Run {
    angle: Vec<f32>,
    slide: Vec<f32>,
    dt: f32,
}

impl Run {
    /// Jump in finite-difference velocity between the frame before and after `frame`.
    fn velocity_jump(&self, frame: usize) -> (f32, f32) {
        let v = |s: &[f32], k: usize| (s[k] - s[k - 1]) / self.dt;
        (
            (v(&self.angle, frame + 1) - v(&self.angle, frame)).abs(),
            (v(&self.slide, frame + 1) - v(&self.slide, frame)).abs(),
        )
    }

    fn position_jump(&self, frame: usize) -> f32 {
        (self.angle[frame + 1] - self.angle[frame]).abs()
    }
}

/// Plays `graph` at `dt`, setting `Go` at `go_at` seconds (and clearing it at `back_at`), and
/// returns the switch frames (fire frames) with the sampled channels.
fn run(
    graph: &AnimGraph,
    clips: &ClipLibrary,
    dt: f32,
    go_at: f32,
    back_at: Option<f32>,
    seconds: f32,
) -> (Run, Vec<usize>) {
    let sk = skeleton();
    let sockets = HashMap::new();
    let mut params = ParameterSet::from_decls(&graph.parameters);
    let mut inst = GraphInstance::from_node(&graph.root);
    let mut out = Pose::from_identity(sk.len());
    let mut scratch = Vec::new();
    let mut result = Run {
        angle: Vec::new(),
        slide: Vec::new(),
        dt,
    };
    let mut switches = Vec::new();
    let frames = (seconds / dt).round() as usize;
    for frame in 0..frames {
        let time = frame as f32 * dt;
        let go = time >= go_at - dt * 0.5 && back_at.is_none_or(|b| time < b - dt * 0.5);
        params.set_bool("Go", go);
        let mut ctx = EvalContext::new(&sk, clips, &params, dt, Entity::PLACEHOLDER, &sockets, scratch);
        evaluate_graph(graph, &mut inst, &mut ctx, &mut out).expect("eval");
        if ctx.state_events.iter().any(|e| e.from.is_some()) {
            switches.push(frame);
        }
        scratch = ctx.finish().scratch;
        let (a, s) = channels(&out);
        result.angle.push(a);
        result.slide.push(s);
    }
    (result, switches)
}

fn library(a: AnimationClip, b: AnimationClip) -> ClipLibrary {
    let mut lib = ClipLibrary::new();
    lib.insert(a);
    lib.insert(b);
    lib
}

/// Source moving *toward* a moving target: A's angle falls, B swings.
fn toward_clips() -> ClipLibrary {
    library(
        clip("A", |t| 1.0 - 0.5 * t, |t| 0.4 - 0.4 * t),
        clip(
            "B",
            |t| 0.3 * (std::f32::consts::TAU * t).sin(),
            |t| -0.2 + 0.1 * (3.0 * t).sin(),
        ),
    )
}

#[test]
fn inertialized_switch_is_continuous_in_position_and_velocity() {
    let clips = toward_clips();
    for blend in ["Inertialize", "InertializeSpring"] {
        let g = graph(blend, 0.3);
        let mut jumps = Vec::new();
        for dt in [1.0 / 120.0, 1.0 / 480.0, 1.0 / 1920.0] {
            let (r, switches) = run(&g, &clips, dt, 0.5, None, 0.7);
            let k = switches[0];
            let (j_angle, j_slide) = r.velocity_jump(k);
            assert!(
                r.position_jump(k) < 2.0 * dt,
                "{blend}: no position pop at dt {dt}"
            );
            jumps.push(j_angle.max(j_slide));
        }
        println!("{blend}: velocity jump at the switch (rad/s, m/s) at dt 1/120, 1/480, 1/1920: {jumps:?}");
        // The velocity jump shrinks with the frame time (continuity) and is small at 1/1920 s.
        assert!(
            jumps[2] < jumps[0] * 0.25,
            "{blend}: velocity jump must vanish with dt: {jumps:?}"
        );
        assert!(jumps[2] < 0.5, "{blend}: velocity jump at dt=1/1920: {jumps:?}");
    }
}

#[test]
fn crossfade_and_cut_are_not_continuous() {
    let clips = toward_clips();
    let dt = 1.0 / 1920.0;
    let (r, switches) = run(&graph("Crossfade", 0.05), &clips, dt, 0.5, None, 0.7);
    let (j, _) = r.velocity_jump(switches[0]);
    println!("Crossfade 0.05 s: velocity jump at the switch at dt 1/1920: {j}");
    assert!(
        j > 10.0,
        "a 0.05 s cross-fade jumps in velocity by ~2·offset/duration: {j}"
    );
    let (r, switches) = run(&graph("Crossfade", 0.0), &clips, dt, 0.5, None, 0.7);
    assert!(r.position_jump(switches[0]) > 0.5, "a cut pops in position");
}

#[test]
fn quintic_clamps_velocity_pointing_away_spring_keeps_it() {
    // A's angle *rises* at the switch while the target sits below: the offset is growing.
    let clips = library(clip("A", |t| 0.5 + 0.5 * t, |_| 0.0), clip("B", |_| 0.0, |_| 0.0));
    let dt = 1.0 / 1920.0;
    let (r, s) = run(&graph("Inertialize", 0.3), &clips, dt, 0.5, None, 0.7);
    let (j, _) = r.velocity_jump(s[0]);
    assert!(r.position_jump(s[0]) < 2.0 * dt, "position continuous");
    assert!(
        (j - 0.5).abs() < 0.1,
        "Bollo clamp: the away-pointing 0.5 rad/s is dropped (no overshoot), got {j}"
    );
    // Never overshoots past the target pose (0) while decaying.
    assert!(r.angle[s[0]..].iter().all(|&a| a > -1e-4));
    let quintic_jump = j;
    let (r, s) = run(&graph("InertializeSpring", 0.3), &clips, dt, 0.5, None, 0.7);
    let (j, _) = r.velocity_jump(s[0]);
    assert!(
        j < 0.25,
        "spring keeps the full velocity (no ~0.5 rad/s drop): {j}"
    );
    assert!(
        j + 0.2 < quintic_jump,
        "spring jump {j} must be clearly smaller than the quintic clamp's {quintic_jump}"
    );
}

#[test]
fn interruption_reinertializes_from_the_current_output() {
    let clips = toward_clips();
    for blend in ["Inertialize", "InertializeSpring"] {
        let g = graph(blend, 0.3);
        let mut jumps = Vec::new();
        for dt in [1.0 / 120.0, 1.0 / 1920.0] {
            // A → B at 0.5 s, interrupted by B → A at 0.6 s (mid-decay).
            let (r, s) = run(&g, &clips, dt, 0.5, Some(0.6), 0.8);
            assert_eq!(s.len(), 2, "{blend}: two switches");
            let k = s[1];
            assert!(
                r.position_jump(k) < 6.0 * dt,
                "{blend}: no pop on interruption at {dt}"
            );
            let (ja, js) = r.velocity_jump(k);
            jumps.push(ja.max(js));
        }
        if blend == "InertializeSpring" {
            assert!(
                jumps[1] < jumps[0] * 0.25,
                "spring: interruption velocity jump must vanish: {jumps:?}"
            );
        } else {
            // The quintic keeps the velocity component that points toward the new target and
            // drops (Bollo's clamp) the part that would carry the pose further away, so its
            // jump is bounded by that component rather than vanishing.
            assert!(
                jumps[1] < 3.0,
                "quintic: bounded interruption velocity jump: {jumps:?}"
            );
        }
    }
    // The cross-fade interruption (existing behaviour) drops the old source: it pops.
    let dt = 1.0 / 1920.0;
    let (r, s) = run(&graph("Crossfade", 0.3), &clips, dt, 0.5, Some(0.6), 0.8);
    let (ja, _) = r.velocity_jump(s[1]);
    assert!(
        ja > 1.0,
        "cross-fade interruption is discontinuous in velocity: {ja}"
    );
}

#[test]
fn debug_snapshot_reports_mode_and_decaying_offset() {
    let clips = toward_clips();
    let g = graph("Inertialize", 0.2);
    let sk = skeleton();
    let sockets = HashMap::new();
    let mut params = ParameterSet::from_decls(&g.parameters);
    let mut inst = GraphInstance::from_node(&g.root);
    let mut out = Pose::from_identity(sk.len());
    let mut scratch = Vec::new();
    let dt = 1.0 / 60.0;
    let mut offsets = Vec::new();
    let mut modes = Vec::new();
    for frame in 0..40 {
        params.set_bool("Go", frame >= 10);
        let mut ctx = EvalContext::new(&sk, &clips, &params, dt, Entity::PLACEHOLDER, &sockets, scratch);
        evaluate_graph(&g, &mut inst, &mut ctx, &mut out).expect("eval");
        scratch = ctx.finish().scratch;
        let debug = graph_debug(&g, &inst, &params);
        let Some(NodeDebugState::StateMachine {
            blend,
            inertial_offset,
            ..
        }) = debug.node("root").map(|n| n.state.clone())
        else {
            panic!("root is a state machine");
        };
        modes.push(blend);
        offsets.push(inertial_offset);
        // Only the target state is evaluated during an inertialized transition.
        if blend.is_some() {
            assert!(!debug.node("root/state:A").expect("A").active);
            assert!(debug.node("root/state:B").expect("B").active);
        }
    }
    assert_eq!(modes[10], Some(TransitionBlend::Inertialize));
    assert!(
        offsets[11] > 0.1,
        "offset applied right after the switch: {:?}",
        &offsets[9..14]
    );
    assert!(offsets[12] < offsets[11]);
    assert_eq!(modes[39], None, "transition completed");
    assert_eq!(offsets[39], 0.0, "settled");
}

#[test]
fn layer_weight_inertialize_eases_a_snapped_weight() {
    let text = r#"(format_version: 2, parameters: [("W", Float(0.0))],
        root: Layer((base: Clip((clip: "A", looping: true, speed: 1.0, sync_group: None)),
            layers: [(graph: Clip((clip: "B", looping: true, speed: 1.0, sync_group: None)),
                mask: (joints: ["child"], include_children: true), mode: Override,
                weight_param: "W", weight_inertialize: 0.2)])))"#;
    let g = AnimGraph::from_ron_str(text).expect("parses");
    let clips = library(clip("A", |_| 1.0, |_| 0.0), clip("B", |_| 0.0, |_| 0.0));
    let sk = skeleton();
    let sockets = HashMap::new();
    let mut params = ParameterSet::from_decls(&g.parameters);
    let mut inst = GraphInstance::from_node(&g.root);
    let mut out = Pose::from_identity(sk.len());
    let mut scratch = Vec::new();
    let mut angles = Vec::new();
    for frame in 0..30 {
        params.set_float("W", if frame >= 5 { 1.0 } else { 0.0 });
        let mut ctx = EvalContext::new(
            &sk,
            &clips,
            &params,
            1.0 / 60.0,
            Entity::PLACEHOLDER,
            &sockets,
            scratch,
        );
        evaluate_graph(&g, &mut inst, &mut ctx, &mut out).expect("eval");
        scratch = ctx.finish().scratch;
        angles.push(channels(&out).0);
    }
    let max_step = angles
        .windows(2)
        .map(|w| (w[1] - w[0]).abs())
        .fold(0.0f32, f32::max);
    // Smootherstep-like ease over 0.2 s: peak ≈ 1.875 / 0.2 s · (1/60 s) ≈ 0.16 rad per frame,
    // against a 1 rad pop without inertialization.
    assert!(
        max_step < 0.2,
        "no pop when the weight snaps 0 → 1: max step {max_step}"
    );
    assert!(angles[29].abs() < 1e-3, "reaches the layer pose");
    let debug = graph_debug(&g, &inst, &params);
    match debug.node("root").map(|n| &n.state) {
        Some(NodeDebugState::Layers { weights }) => assert!((weights[0] - 1.0).abs() < 1e-4),
        other => panic!("layer debug {other:?}"),
    }
}

/// DO27's pilot body graph (read from the read-only content drop when
/// `REGIA_DO27_CONTENT` is set): loads as v1 with every transition a cross-fade, and with
/// all 37 switched to `Inertialize` it round-trips and evaluates.
#[test]
fn do27_pilot_body_graph_loads_and_switches_to_inertialize() {
    let Ok(root) = regia_core::brand::env_var("REGIA_DO27_CONTENT") else {
        eprintln!("REGIA_DO27_CONTENT not set; skipping");
        return;
    };
    let path = std::path::Path::new(&root).join("pilots/anims/body.animgraph.ron");
    let Ok(text) = std::fs::read_to_string(&path) else {
        eprintln!("{} missing; skipping", path.display());
        return;
    };
    let mut graph = AnimGraph::from_ron_str(&text).expect("DO27 body graph loads (v1 → v2)");
    let AnimNode::Layer(layer) = &mut graph.root else {
        panic!("body graph root is a layer stack");
    };
    let AnimNode::StateMachine(sm) = layer.base.as_mut() else {
        panic!("body graph base is a state machine");
    };
    assert!(sm.transitions.len() >= 30, "{}", sm.transitions.len());
    assert!(
        sm.transitions
            .iter()
            .all(|t| t.blend == TransitionBlend::Crossfade)
    );
    for t in &mut sm.transitions {
        t.blend = TransitionBlend::Inertialize;
    }
    let written = graph.to_ron_string().expect("write");
    let back = AnimGraph::from_ron_str(&written).expect("reparse");
    assert_eq!(written.matches("blend: Inertialize").count(), sm_len(&back));
}

fn sm_len(graph: &AnimGraph) -> usize {
    match &graph.root {
        AnimNode::Layer(l) => match l.base.as_ref() {
            AnimNode::StateMachine(sm) => sm.transitions.len(),
            _ => 0,
        },
        _ => 0,
    }
}

/// Stand-in for DO27's pilot graph features: hip → knee → foot leg plus an arm, under a
/// masked Override aim layer, with TwoBoneIK on the leg after the machine and a synced
/// BlendSpace2D target state whose clips carry XZ root motion.
fn feature_skeleton() -> Skeleton {
    let at = |x: f32, y: f32| LocalBind {
        translation: Vec3::new(x, y, 0.0),
        ..LocalBind::default()
    };
    Skeleton::from_local_bind(
        vec![
            "root".into(),
            "hip".into(),
            "knee".into(),
            "foot".into(),
            "arm".into(),
        ],
        vec![None, Some(0), Some(1), Some(2), Some(0)],
        vec![
            LocalBind::default(),
            at(0.1, 1.0),
            at(0.0, -0.5),
            at(0.0, -0.5),
            at(0.3, 0.5),
        ],
    )
    .expect("valid skeleton")
}

/// A looping clip on the feature rig: root X = `speed · t` (XZ root motion when `speed` ≠ 0),
/// hip/knee bend about X and arm swing about X from `angles(t)`.
fn feature_clip(name: &str, duration: f32, speed: f32, angles: impl Fn(f32) -> [f32; 3]) -> AnimationClip {
    let n = (duration * 1000.0) as usize;
    let times: Vec<f32> = (0..=n).map(|i| i as f32 / 1000.0).collect();
    let rot = |k: usize| TrackCurve::Animated {
        times: times.clone(),
        values: times
            .iter()
            .map(|&t| Quat::from_rotation_x(angles(t)[k]))
            .collect(),
        exact_rotation: true,
    };
    let mut c = AnimationClip::empty(name, duration);
    c.tracks.push(JointTrack {
        joint: 0,
        translations: TrackCurve::Animated {
            times: times.clone(),
            values: times.iter().map(|&t| Vec3::new(speed * t, 0.0, 0.0)).collect(),
            exact_rotation: false,
        },
        rotations: TrackCurve::Static(Quat::IDENTITY),
        scales: TrackCurve::Static(Vec3::ONE),
    });
    for (joint, k, y) in [(1u32, 0usize, 0.0f32), (2, 1, -0.5), (4, 2, 0.5)] {
        c.tracks.push(JointTrack {
            joint,
            translations: TrackCurve::Static(if joint == 1 {
                Vec3::new(0.1, 1.0, 0.0)
            } else if joint == 4 {
                Vec3::new(0.3, y, 0.0)
            } else {
                Vec3::new(0.0, y, 0.0)
            }),
            rotations: rot(k),
            scales: TrackCurve::Static(Vec3::ONE),
        });
    }
    if speed != 0.0 {
        c.root_motion = Some(regia_animation::clip::RootMotionTrack {
            joint: 0,
            mode: regia_animation::clip::RootMotionMode::XZ,
        });
    }
    c
}

fn feature_graph(blend: &str, duration: f32) -> AnimGraph {
    let text = format!(
        r#"(format_version: 2,
        parameters: [("Go", Bool(false)), ("X", Float(0.6)), ("Y", Float(0.3)), ("Aim", Float(0.0)),
                     ("Foot", Vec3((0.1, 0.15, 0.25)))],
        root: Layer((
            base: TwoBoneIK((
                inner: StateMachine((initial: "Idle",
                    states: [
                        ("Idle", Clip((clip: "Idle", looping: true, speed: 1.0, sync_group: None))),
                        ("Move", BlendSpace2D((space: (param_x: "X", param_y: "Y",
                            samples: [(point: (0.0, 0.0), clip: "W0"), (point: (1.0, 0.0), clip: "W1"),
                                      (point: (0.0, 1.0), clip: "W2")],
                            triangles: [(0, 1, 2)]), sync_group: Some("gait")))),
                    ],
                    transitions: [(from: Some("Idle"), to: "Move", conditions: [Bool(param: "Go", value: true)],
                        duration: {duration}, curve: Linear, interruptible: true, blend: {blend})])),
                chain: ("hip", "knee", "foot"), target: Param("Foot"), pole: None, weight_param: None)),
            layers: [(graph: Clip((clip: "Aim", looping: true, speed: 1.0, sync_group: None)),
                      mask: (joints: ["arm"], include_children: true), mode: Override,
                      weight_param: "Aim", weight_inertialize: 0.15)])))"#
    );
    AnimGraph::from_ron_str(&text).expect("feature graph parses")
}

struct FeatureRun {
    poses: Vec<Pose>,
    root_motion: Vec<Vec3>,
    switch: usize,
}

fn run_features(graph: &AnimGraph, dt: f32, go_at: f32, aim_at: f32, seconds: f32) -> FeatureRun {
    use std::f32::consts::TAU;
    let sk = feature_skeleton();
    let mut clips = ClipLibrary::new();
    clips.insert(feature_clip("Idle", 2.0, 0.0, |t| {
        [0.1 * (TAU * t / 2.0).sin(), -0.2, 0.0]
    }));
    for (name, len, speed) in [("W0", 1.0, 1.0), ("W1", 1.2, 2.0), ("W2", 1.4, 1.5)] {
        clips.insert(feature_clip(name, len, speed, move |t| {
            let phase = TAU * t / len;
            [0.5 * phase.sin(), -0.6 + 0.3 * phase.cos(), 0.6 * phase.cos()]
        }));
    }
    clips.insert(feature_clip("Aim", 1.0, 0.0, |_| [0.0, 0.0, 1.2]));
    let sockets = HashMap::new();
    let mut params = ParameterSet::from_decls(&graph.parameters);
    let mut inst = GraphInstance::from_node(&graph.root);
    let mut out = Pose::from_identity(sk.len());
    let mut scratch = Vec::new();
    let mut result = FeatureRun {
        poses: Vec::new(),
        root_motion: Vec::new(),
        switch: 0,
    };
    for frame in 0..(seconds / dt).round() as usize {
        let time = frame as f32 * dt;
        params.set_bool("Go", time >= go_at - dt * 0.5);
        params.set_float("Aim", if time >= aim_at - dt * 0.5 { 1.0 } else { 0.0 });
        let mut ctx = EvalContext::new(&sk, &clips, &params, dt, Entity::PLACEHOLDER, &sockets, scratch);
        evaluate_graph(graph, &mut inst, &mut ctx, &mut out).expect("eval");
        if ctx.state_events.iter().any(|e| e.from.is_some()) {
            result.switch = frame;
        }
        result.root_motion.push(ctx.root_motion.0);
        scratch = ctx.finish().scratch;
        result.poses.push(out.clone());
    }
    result
}

/// Largest local rotation change of any joint between frames `k - 1` and `k`.
fn rotation_step(poses: &[Pose], k: usize, joints: &[usize]) -> f32 {
    joints
        .iter()
        .map(|&j| poses[k].rotations[j].angle_between(poses[k - 1].rotations[j]))
        .fold(0.0, f32::max)
}

#[test]
fn inertialize_composes_with_blendspace2d_masked_layer_ik_and_root_motion() {
    let dt = 1.0 / 120.0;
    let (go_at, aim_at) = (0.5, 1.0);
    let inertial = run_features(&feature_graph("Inertialize", 0.2), dt, go_at, aim_at, 1.5);
    let cut = run_features(&feature_graph("Crossfade", 0.0), dt, go_at, aim_at, 1.5);
    let s = inertial.switch;
    assert!(
        s > 0 && s == cut.switch,
        "both switch on the same frame: {s} vs {}",
        cut.switch
    );

    // Pose continuity through the IK'd leg and the (not yet weighted) aim layer's arm.
    let joints = [1, 2, 3, 4];
    let before = (s - 3..s)
        .map(|k| rotation_step(&inertial.poses, k, &joints))
        .fold(0.0, f32::max);
    // The state event fires on the frame the transition is taken; the pose steps on that frame or
    // the next, so the switch step is the larger of the two.
    let switch_step =
        |poses: &[Pose]| rotation_step(poses, s, &joints).max(rotation_step(poses, s + 1, &joints));
    let at_switch = switch_step(&inertial.poses);
    let cut_switch = switch_step(&cut.poses);
    assert!(
        cut_switch > 0.3,
        "the states differ: a cut pops by {cut_switch} rad"
    );
    assert!(
        at_switch < 2.0 * before + 0.02,
        "inertialized switch steps {at_switch} rad (frames before: {before}, cut: {cut_switch})"
    );

    // Root motion comes from the target state alone once the switch has taken effect, exactly as
    // for a cut (the velocity probe leaves no root motion behind).
    for k in s + 1..inertial.root_motion.len() {
        let (a, b) = (inertial.root_motion[k], cut.root_motion[k]);
        assert!(
            (a - b).length() < 1e-6,
            "frame {k}: root motion {a:?} vs cut {b:?}"
        );
        assert!(a.x > 0.0, "the synced blend space moves the root");
    }

    // The aim layer owns the masked arm: its snapped weight eases in without a pop and, once
    // settled, the arm is exactly the aim pose whatever the machine's offset was.
    let aim_frame = (aim_at / dt).round() as usize;
    let arm_step = rotation_step(&inertial.poses, aim_frame, &[4]);
    assert!(
        arm_step < 0.05,
        "aim layer weight eases in: first step {arm_step} rad"
    );
    let last = inertial.poses.last().expect("frames");
    assert!(last.rotations[4].angle_between(Quat::from_rotation_x(1.2)) < 1e-4);

    // TwoBoneIK still plants the foot on its target after the inertialized machine.
    let sk = feature_skeleton();
    for k in [s, s + 6, s + 12] {
        let foot = sk.globals(&inertial.poses[k], None)[3].w_axis.truncate();
        assert!(
            (foot - Vec3::new(0.1, 0.15, 0.25)).length() < 1e-3,
            "frame {k}: foot at {foot:?}"
        );
    }
}
