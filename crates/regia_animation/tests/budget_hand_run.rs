// SPDX-License-Identifier: MPL-2.0
//! Spec 17 § 2 performance budget, measured by hand (ADR-5614): 100 characters × 60 joints ×
//! a 3-layer graph through `AnimationPlugin` (evaluate + apply). The spec's target is ≤ 2 ms on
//! desktop.
//!
//! This is hand-run timing, not part of the gate, and it asserts nothing about time. Run:
//! `cargo test -p regia_animation --release --test budget_hand_run -- --ignored --nocapture`
//! on an idle machine. The always-on test below checks only that the scene animates, so the
//! timing run measures real work.

use std::sync::Arc;
use std::time::{Duration, Instant};

use regia_animation::blendspace::{BlendSample1D, BlendSpace1D};
use regia_animation::clip::{AnimationClip, JointTrack, TrackCurve};
use regia_animation::components::spawn_skeleton;
use regia_animation::graph::{BlendSpace1DNode, ClipNode, JointMaskDef, LayerEntry, LayerMode, LayerNode};
use regia_animation::params::{ParamDecl, ParamValue};
use regia_animation::prelude::*;
use regia_animation::skeleton::LocalBind;
use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_scene::Transform;

const JOINTS: usize = 60;

/// A 60-joint chain, 10 cm per bone.
fn skeleton() -> Skeleton {
    let names = (0..JOINTS).map(|i| format!("j{i}")).collect();
    let parents = (0..JOINTS).map(|i| i.checked_sub(1).map(|p| p as u32)).collect();
    let binds = (0..JOINTS)
        .map(|_| LocalBind {
            translation: Vec3::new(0.0, 0.1, 0.0),
            ..LocalBind::default()
        })
        .collect();
    Skeleton::from_local_bind(names, parents, binds).expect("skeleton")
}

/// A clip that moves and turns every joint: `swing` radians about Z over `duration`.
fn clip(name: &str, duration: f32, swing: f32) -> AnimationClip {
    let mut clip = AnimationClip::empty(name, duration);
    clip.tracks = (0..JOINTS as u32)
        .map(|joint| JointTrack {
            joint,
            translations: TrackCurve::Animated {
                times: vec![0.0, duration * 0.5, duration],
                values: vec![
                    Vec3::new(0.0, 0.1, 0.0),
                    Vec3::new(0.01, 0.1, 0.0),
                    Vec3::new(0.0, 0.1, 0.0),
                ],
                exact_rotation: false,
            },
            rotations: TrackCurve::Animated {
                times: vec![0.0, duration * 0.5, duration],
                values: vec![Quat::IDENTITY, Quat::from_rotation_z(swing), Quat::IDENTITY],
                exact_rotation: false,
            },
            scales: TrackCurve::Static(Vec3::ONE),
        })
        .collect();
    clip
}

fn clip_node(name: &str) -> Box<AnimNode> {
    Box::new(AnimNode::Clip(ClipNode {
        clip: name.into(),
        looping: true,
        speed: 1.0,
        sync_group: None,
    }))
}

/// Locomotion blend space (idle / walk / run) under an upper-body override layer and a
/// whole-body additive layer: three layers.
fn graph() -> (AnimGraph, ClipLibrary) {
    let mut clips = ClipLibrary::new();
    for (name, duration, swing) in [
        ("idle", 2.0, 0.05),
        ("walk", 1.0, 0.2),
        ("run", 0.6, 0.35),
        ("wave", 1.5, 0.6),
        ("breathe", 3.0, 0.02),
    ] {
        clips.insert(clip(name, duration, swing));
    }
    let locomotion = AnimNode::BlendSpace1D(BlendSpace1DNode {
        space: BlendSpace1D {
            param: "Speed".into(),
            samples: [(0.0, "idle"), (1.5, "walk"), (4.0, "run")]
                .iter()
                .map(|&(value, clip)| BlendSample1D {
                    value,
                    clip: clip.into(),
                })
                .collect(),
        },
        sync_group: Some("gait".into()),
    });
    let graph = AnimGraph {
        parameters: vec![
            ParamDecl("Speed".into(), ParamValue::Float(2.0)),
            ParamDecl("Wave".into(), ParamValue::Float(0.7)),
            ParamDecl("Breathe".into(), ParamValue::Float(1.0)),
        ],
        root: AnimNode::Layer(LayerNode {
            base: Box::new(locomotion),
            layers: vec![
                LayerEntry {
                    graph: clip_node("wave"),
                    mask: JointMaskDef {
                        joints: vec!["j30".into()],
                        include_children: true,
                    },
                    mode: LayerMode::Override,
                    weight_param: "Wave".into(),
                    weight_inertialize: 0.0,
                },
                LayerEntry {
                    graph: clip_node("breathe"),
                    mask: JointMaskDef {
                        joints: vec!["j0".into()],
                        include_children: true,
                    },
                    mode: LayerMode::Additive,
                    weight_param: "Breathe".into(),
                    weight_inertialize: 0.0,
                },
            ],
        }),
    };
    (graph, clips)
}

/// An app with `rigs` animated characters and a 1/60 s manual step, after one warm-up update.
fn scene(rigs: usize) -> (App, Vec<Entity>) {
    let mut app = App::new();
    app.add_plugins((regia_app::MinimalPlugins, AnimationPlugin));
    app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
        Duration::from_micros(16_667),
    ));
    app.update();
    let skeleton = Arc::new(skeleton());
    let (graph, clips) = graph();
    let mut tips = Vec::with_capacity(rigs);
    for i in 0..rigs {
        let binding = spawn_skeleton(app.world_mut(), skeleton.clone());
        let tip = binding.joints[JOINTS - 1];
        let mut animator = Animator::graph(graph.clone(), clips.clone());
        // Spread the speeds so the blend space weights differ per rig.
        animator.set("Speed", (i % 9) as f32 * 0.5);
        app.world_mut().spawn((
            Transform::from_translation(Vec3::new(i as f32, 0.0, 0.0)),
            binding,
            animator,
        ));
        tips.push(tip);
    }
    app.update();
    (app, tips)
}

/// The scene the timing run measures really animates: the chain tips move between frames.
#[test]
fn the_budget_scene_animates_every_rig() {
    let (mut app, tips) = scene(4);
    let before: Vec<Quat> = tips
        .iter()
        .map(|&tip| app.world().get::<Transform>(tip).expect("joint").rotation)
        .collect();
    for _ in 0..10 {
        app.update();
    }
    for (i, &tip) in tips.iter().enumerate() {
        let after = app.world().get::<Transform>(tip).expect("joint").rotation;
        assert!(after.angle_between(before[i]) > 1e-3, "rig {i} tip did not move");
    }
}

/// Hand-run timing: 100 rigs, 300 frames after 30 warm-up frames; prints the mean and the worst
/// frame of `App::update` (the animation systems plus the minimal schedule around them).
#[test]
#[ignore = "hand-run timing"]
fn animation_budget_hand_run() {
    const RIGS: usize = 100;
    const FRAMES: usize = 300;
    let (mut app, _) = scene(RIGS);
    for _ in 0..30 {
        app.update();
    }
    let mut worst = Duration::ZERO;
    let start = Instant::now();
    for _ in 0..FRAMES {
        let frame = Instant::now();
        app.update();
        worst = worst.max(frame.elapsed());
    }
    let mean = start.elapsed() / FRAMES as u32;
    println!(
        "animation budget: {RIGS} rigs x {JOINTS} joints x 3 layers: mean {:.3} ms, worst {:.3} ms per frame (spec 17 target <= 2 ms desktop)",
        mean.as_secs_f64() * 1e3,
        worst.as_secs_f64() * 1e3
    );
}
