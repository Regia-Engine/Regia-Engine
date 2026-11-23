// SPDX-License-Identifier: MPL-2.0
//! The `ChainIK` and `BodyAim` graph nodes (ADR-8475) allocate nothing per frame once warm: their
//! joints resolve once per rig and their solvers run on kept buffers.

use std::collections::HashMap;

use regia_animation::Skeleton;
use regia_animation::aim_ik::{AimChain, AimScratch, solve_body_aim_with};
use regia_animation::chain_ik::{
    ChainIkScratch, ChainIkSettings, ChainSolver, JointChain, JointLimit, solve_chain,
};
use regia_animation::clip::AnimationClip;
use regia_animation::eval::{ClipLibrary, EvalContext, EvalReuse, GraphInstance, evaluate_graph};
use regia_animation::graph::{
    AnimGraph, AnimNode, BodyAimNode, ChainIKNode, ChainLimits, ClipNode, IkTarget,
};
use regia_animation::params::{ParamDecl, ParamValue};
use regia_animation::pose::Pose;
use regia_animation::skeleton::LocalBind;
use regia_core::math::{Mat4, Vec3};
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

fn ik_graph() -> AnimGraph {
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

/// Allocations of `frames` warm evaluations of `graph`.
fn allocations_of(graph: &AnimGraph, frames: usize) -> u64 {
    let skeleton = rig();
    let mut clips = ClipLibrary::new();
    clips.insert(AnimationClip::empty("idle", 1.0));
    let mut params = graph.default_parameters();
    let sockets = HashMap::new();
    let mut instance = GraphInstance::from_node(&graph.root);
    let mut reuse = EvalReuse::default();
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
    stats.allocs
}

#[test]
fn chain_ik_and_body_aim_nodes_add_no_allocation_per_frame() {
    let plain = AnimGraph {
        parameters: ik_graph().parameters,
        root: clip(),
    };
    let base = allocations_of(&plain, 120);
    let with_ik = allocations_of(&ik_graph(), 120);
    assert_eq!(
        with_ik,
        base,
        "the IK nodes added {} allocations over 120 warm frames",
        with_ik as i64 - base as i64
    );
}

#[test]
fn kept_buffers_make_the_solvers_allocation_free() {
    let skeleton = rig();
    let chain = JointChain::between(&skeleton, "t0", "t5")
        .expect("tail")
        .with_uniform_limit(JointLimit::Cone {
            max_swing: 0.6,
            min_twist: -0.2,
            max_twist: 0.2,
        });
    let aim = AimChain {
        joints: vec![7, 8, 9],
        shares: vec![1.0, 1.0, 2.0],
        limits: Vec::new(),
        aim_joint: 10,
        aim_axis: Vec3::NEG_Z,
    };
    let mut chain_scratch = ChainIkScratch::default();
    let mut aim_scratch = AimScratch::default();
    let mut fk: Vec<Mat4> = Vec::new();
    let bind = skeleton.bind_pose_pose();
    let mut pose = bind.clone();
    let mut run = |frame: usize, pose: &mut Pose| {
        // `Pose::copy_from` clones (three allocations); the solvers only turn rotations.
        pose.rotations.copy_from_slice(&bind.rotations);
        let a = frame as f32 * 0.1;
        for solver in [ChainSolver::Fabrik, ChainSolver::Ccd] {
            solve_chain(
                &skeleton,
                pose,
                &chain,
                Vec3::new(0.3 * a.cos(), -0.1, 0.3 * a.sin() - 0.2),
                Some(Vec3::X),
                solver,
                ChainIkSettings::default(),
                &mut chain_scratch,
            );
        }
        solve_body_aim_with(
            &skeleton,
            pose,
            &aim,
            Vec3::new(4.0 * a.sin(), 1.5, -6.0),
            &[],
            0.8,
            4,
            &mut fk,
            &mut aim_scratch,
        );
    };
    for frame in 0..4 {
        run(frame, &mut pose);
    }
    let ((), stats) = alloc_counter::measure(|| {
        for frame in 4..204 {
            run(frame, &mut pose);
        }
    });
    assert_eq!(
        stats.allocs, 0,
        "warm solves allocated {} times ({} bytes)",
        stats.allocs, stats.bytes
    );
}
