// SPDX-License-Identifier: MPL-2.0
//! IK residual capture (`anim.ik_report`, ADR-8708) allocates nothing per frame once warm: with the
//! rig's IK debug flag on, a graph of `TwoBoneIK`, `LookAt`, `ChainIK` and `BodyAim` nodes makes as
//! many allocations as with the flag off (the captures reuse one kept buffer).

use std::collections::HashMap;

use regia_animation::Skeleton;
use regia_animation::chain_ik::{ChainSolver, JointLimit};
use regia_animation::clip::AnimationClip;
use regia_animation::eval::{ClipLibrary, EvalContext, EvalReuse, GraphInstance, evaluate_graph};
use regia_animation::graph::{
    AnimGraph, AnimNode, BodyAimNode, ChainIKNode, ChainLimits, ClipNode, IkTarget, LookAtNode, TwoBoneIKNode,
};
use regia_animation::params::{ParamDecl, ParamValue};
use regia_animation::pose::Pose;
use regia_animation::skeleton::LocalBind;
use regia_core::math::Vec3;
use regia_ecs::prelude::Entity;
use regia_test_utils::alloc_counter::{self, CountingAlloc};

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc::new();

/// `hips`, a six-joint tail `t0..t5` and a three-joint neck `n0..n2` with a `head` above.
fn rig() -> Skeleton {
    let mut names = vec!["hips".to_string()];
    let mut parents = vec![None];
    let mut binds = vec![Vec3::new(0.0, 0.5, 0.0)];
    for i in 0..6u32 {
        names.push(format!("t{i}"));
        parents.push(Some(i));
        binds.push(Vec3::new(0.0, -0.15, -0.05));
    }
    for i in 0..3u32 {
        names.push(format!("n{i}"));
        parents.push(Some(if i == 0 { 0 } else { 6 + i }));
        binds.push(Vec3::new(0.0, 0.2, 0.0));
    }
    names.push("head".into());
    parents.push(Some(9));
    binds.push(Vec3::new(0.0, 0.1, -0.1));
    let binds = binds
        .into_iter()
        .map(|translation| LocalBind {
            translation,
            ..LocalBind::default()
        })
        .collect();
    Skeleton::from_local_bind(names, parents, binds).expect("skeleton")
}

fn clip() -> AnimNode {
    AnimNode::Clip(ClipNode {
        clip: "idle".into(),
        looping: true,
        speed: 1.0,
        sync_group: None,
    })
}

fn aim_graph() -> AnimGraph {
    AnimGraph {
        parameters: vec![
            ParamDecl("Tail".into(), ParamValue::Vec3(Vec3::ZERO)),
            ParamDecl("Look".into(), ParamValue::Vec3(Vec3::ZERO)),
            ParamDecl("Pole".into(), ParamValue::Vec3(Vec3::new(1.0, 0.0, 0.0))),
        ],
        root: AnimNode::BodyAim(BodyAimNode {
            inner: Box::new(AnimNode::ChainIK(ChainIKNode {
                inner: Box::new(clip()),
                root: "t0".into(),
                tip: "t5".into(),
                solver: ChainSolver::Fabrik,
                target: IkTarget::Param("Tail".into()),
                pole: Some("Pole".into()),
                limits: Some(ChainLimits::Uniform(JointLimit::Cone {
                    max_swing: 0.6,
                    min_twist: -0.2,
                    max_twist: 0.2,
                })),
                iterations: 24,
                tolerance: 1e-3,
                weight_param: None,
            })),
            joints: vec![("n0".into(), 1.0), ("n1".into(), 1.0), ("n2".into(), 2.0)],
            aim_joint: "head".into(),
            axis: Vec3::NEG_Z,
            target: IkTarget::Param("Look".into()),
            limits: None,
            passes: 4,
            weight_param: None,
        }),
    }
}

/// `TwoBoneIK` (tail tip) around `LookAt` (head) around the `BodyAim` and `ChainIK` graph.
fn ik_graph() -> AnimGraph {
    let inner = aim_graph();
    let mut parameters = inner.parameters;
    parameters.push(ParamDecl(
        "Tip".into(),
        ParamValue::Vec3(Vec3::new(0.1, -0.2, -0.1)),
    ));
    AnimGraph {
        parameters,
        root: AnimNode::TwoBoneIK(TwoBoneIKNode {
            inner: Box::new(AnimNode::LookAt(LookAtNode {
                inner: Box::new(inner.root),
                joint: "head".into(),
                target: IkTarget::Param("Look".into()),
                max_yaw: 1.0,
                max_pitch: 0.6,
                weight_param: None,
            })),
            chain: ["t3".into(), "t4".into(), "t5".into()],
            target: IkTarget::Param("Tip".into()),
            pole: None,
            weight_param: None,
        }),
    }
}

/// Allocations of `frames` warm evaluations of `graph`, with the IK debug flag `debug`, and the
/// captures the rig made.
fn allocations_of(graph: &AnimGraph, frames: usize, debug: bool) -> (u64, u64) {
    let skeleton = rig();
    let mut clips = ClipLibrary::new();
    clips.insert(AnimationClip::empty("idle", 1.0));
    let mut params = graph.default_parameters();
    let sockets = HashMap::new();
    let mut instance = GraphInstance::from_node(&graph.root);
    let mut reuse = EvalReuse::default();
    reuse.set_ik_debug(debug);
    let mut scratch: Vec<Pose> = Vec::new();
    let mut pose = Pose::from_identity(skeleton.len());
    let mut step = |frame: usize, params: &mut regia_animation::params::ParameterSet| {
        let a = frame as f32 * 0.1;
        params.set_vec3("Tail", Vec3::new(0.3 * a.cos(), -0.1, 0.3 * a.sin() - 0.2));
        params.set_vec3("Look", Vec3::new(4.0 * a.sin(), 1.5, -6.0));
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
    (stats.allocs, reuse.ik_debug_captures())
}

#[test]
fn ik_capture_adds_no_allocation_per_frame() {
    let (off, off_captures) = allocations_of(&ik_graph(), 120, false);
    let (on, on_captures) = allocations_of(&ik_graph(), 120, true);
    assert_eq!(off_captures, 0);
    assert_eq!(on_captures, 4 * 128, "four nodes, 128 frames");
    assert_eq!(
        on,
        off,
        "IK capture added {} allocations over 120 warm frames",
        on as i64 - off as i64
    );
}
