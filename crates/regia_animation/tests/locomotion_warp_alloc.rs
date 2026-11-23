// SPDX-License-Identifier: MPL-2.0
//! The `StrideWarp` and `OrientationWarp` graph nodes (ADR-8540) allocate nothing per frame once
//! warm: joints resolve once per rig and the warp code runs on buffers kept in the node's state.

use std::collections::HashMap;

use regia_animation::blendspace::{BlendSample1D, BlendSpace1D};
use regia_animation::demo_rig::{DemoGait, DemoRig, demo_humanoid, demo_locomotion};
use regia_animation::eval::{ClipLibrary, EvalContext, EvalReuse, GraphInstance, evaluate_graph};
use regia_animation::graph::{AnimGraph, AnimNode, BlendSpace1DNode, OrientationWarpNode, StrideWarpNode};
use regia_animation::motion_matching::warp::{StrideMode, StrideWarpSettings};
use regia_animation::params::{ParamDecl, ParamValue};
use regia_animation::pose::Pose;
use regia_ecs::prelude::Entity;
use regia_test_utils::alloc_counter::{self, CountingAlloc};

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc::new();

fn locomotion() -> Box<AnimNode> {
    Box::new(AnimNode::BlendSpace1D(BlendSpace1DNode {
        space: BlendSpace1D {
            param: "Speed".into(),
            samples: vec![
                BlendSample1D {
                    value: 1.3,
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
        ParamDecl("Speed".into(), ParamValue::Float(2.0)),
        ParamDecl("GroundSpeed".into(), ParamValue::Float(2.5)),
        ParamDecl("Heading".into(), ParamValue::Float(0.4)),
    ]
}

fn warp_graph(mode: StrideMode) -> AnimGraph {
    AnimGraph {
        parameters: parameters(),
        root: AnimNode::OrientationWarp(OrientationWarpNode {
            inner: Box::new(AnimNode::StrideWarp(StrideWarpNode {
                inner: locomotion(),
                ground_speed_param: "GroundSpeed".into(),
                settings: StrideWarpSettings {
                    ratio_max: 1.5,
                    mode,
                    ..StrideWarpSettings::graph_default()
                },
                legs: vec![
                    ["left_thigh".into(), "left_calf".into(), "left_foot".into()],
                    ["right_thigh".into(), "right_calf".into(), "right_foot".into()],
                ],
                pelvis: "hips".into(),
                stride_length: 0.5,
            })),
            heading_param: "Heading".into(),
            pelvis: "hips".into(),
            spine: vec![("spine".into(), 0.5), ("chest".into(), 0.5)],
            strength: 1.0,
            max_delta: 0.8,
        }),
    }
}

/// Allocations of `frames` warm evaluations of `graph` (the heading and ground speed change
/// every frame, so the warps do work every frame).
fn allocations_of(graph: &AnimGraph, frames: usize) -> u64 {
    let skeleton = demo_humanoid(&DemoRig::default()).expect("demo rig");
    let mut clips = ClipLibrary::new();
    for clip in demo_locomotion(&skeleton, &DemoGait::standard()) {
        clips.insert(clip);
    }
    let mut params = graph.default_parameters();
    let sockets = HashMap::new();
    let mut instance = GraphInstance::from_node(&graph.root);
    let mut reuse = EvalReuse::default();
    let mut scratch: Vec<Pose> = Vec::new();
    let mut pose = Pose::from_identity(skeleton.len());
    let mut step = |frame: usize, params: &mut regia_animation::params::ParameterSet| {
        let a = frame as f32 * 0.05;
        params.set_float("Heading", 0.6 * a.sin());
        params.set_float("GroundSpeed", 2.5 + 0.3 * a.cos());
        let mut ctx = EvalContext::new(
            &skeleton,
            &clips,
            params,
            1.0 / 60.0,
            Entity::PLACEHOLDER,
            &sockets,
            std::mem::take(&mut scratch),
        )
        .with_reuse(std::mem::take(&mut reuse));
        evaluate_graph(graph, &mut instance, &mut ctx, &mut pose).expect("evaluate");
        reuse = ctx.take_reuse();
        scratch = ctx.finish().scratch;
    };
    for frame in 0..8 {
        step(frame, &mut params);
    }
    let ((), stats) = alloc_counter::measure(|| {
        for frame in 8..8 + frames {
            step(frame, &mut params);
        }
    });
    stats.allocs
}

#[test]
fn warp_nodes_add_no_allocation_per_frame() {
    let plain = AnimGraph {
        parameters: parameters(),
        root: *locomotion(),
    };
    let base = allocations_of(&plain, 120);
    for mode in [StrideMode::Pose, StrideMode::Rate] {
        let with_warps = allocations_of(&warp_graph(mode), 120);
        println!("warp alloc mode={mode:?} plain={base} warped={with_warps}");
        assert_eq!(
            with_warps,
            base,
            "{mode:?}: the warp nodes added {} allocations over 120 warm frames",
            with_warps as i64 - base as i64
        );
    }
}

/// Evidence only (run with `--ignored --nocapture` in release): the per-frame cost of the demo
/// blend space alone and under both warp nodes, on this machine. Prints, asserts nothing.
#[test]
#[ignore = "timing evidence, not a check"]
fn warp_nodes_cost_per_frame() {
    let plain = AnimGraph {
        parameters: parameters(),
        root: *locomotion(),
    };
    let frames = 4000;
    for (label, graph) in [
        ("blend_space", plain),
        ("pose_warps", warp_graph(StrideMode::Pose)),
        ("rate_warps", warp_graph(StrideMode::Rate)),
    ] {
        let mut best = f64::MAX;
        for _ in 0..5 {
            let start = std::time::Instant::now();
            let _ = allocations_of(&graph, frames);
            best = best.min(start.elapsed().as_secs_f64());
        }
        println!(
            "warp_cost {label}: {:.2} us/frame (best of 5, {frames} frames + 8 warm-up, demo humanoid)",
            best * 1e6 / (frames + 8) as f64
        );
    }
}
