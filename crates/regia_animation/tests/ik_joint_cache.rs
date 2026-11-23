// SPDX-License-Identifier: MPL-2.0
//! Two-bone IK resolves its chain once and reuses one global buffer.
//!
//! The second evaluation of a 40-joint rig must not scan joint names or allocate another
//! skeleton-sized matrix buffer for the solve.

use std::collections::HashMap;

use regia_animation::Skeleton;
use regia_animation::clip::AnimationClip;
use regia_animation::eval::{ClipLibrary, EvalContext, EvalReuse, GraphInstance, evaluate_graph};
use regia_animation::graph::{AnimGraph, AnimNode, ClipNode, IkTarget, TwoBoneIKNode};
use regia_animation::hot_count::{self, globals_fills, joint_index_calls};
use regia_animation::params::{ParamDecl, ParamValue};
use regia_animation::pose::Pose;
use regia_animation::skeleton::LocalBind;
use regia_core::math::Vec3;
use regia_ecs::prelude::Entity;

fn arm(n: usize) -> Skeleton {
    let mut names = Vec::with_capacity(n);
    let mut parents = Vec::with_capacity(n);
    let mut binds = Vec::with_capacity(n);
    for i in 0..n {
        names.push(format!("j{i}"));
        parents.push(if i == 0 { None } else { Some((i - 1) as u32) });
        binds.push(LocalBind {
            translation: Vec3::new(0.0, 0.1, 0.0),
            ..LocalBind::default()
        });
    }
    Skeleton::from_local_bind(names, parents, binds).expect("skeleton")
}

#[test]
fn the_second_two_bone_step_does_not_rescan_or_refill_globals() {
    let _guard = hot_count::lock_for_test();
    let skeleton = arm(40);
    let graph = AnimGraph {
        parameters: vec![ParamDecl(
            "Hand".into(),
            ParamValue::Vec3(Vec3::new(0.0, 0.3, 0.2)),
        )],
        root: AnimNode::TwoBoneIK(TwoBoneIKNode {
            inner: Box::new(AnimNode::Clip(ClipNode {
                clip: "idle".into(),
                looping: true,
                speed: 1.0,
                sync_group: None,
            })),
            chain: ["j0".into(), "j1".into(), "j2".into()],
            target: IkTarget::Param("Hand".into()),
            pole: None,
            weight_param: None,
        }),
    };
    let mut clips = ClipLibrary::new();
    clips.insert(AnimationClip::empty("idle", 1.0));
    let params = graph.default_parameters();
    let mut instance = GraphInstance::from_node(&graph.root);
    let sockets = HashMap::new();
    let mut reuse = EvalReuse::default();
    let mut step = |scratch: Vec<Pose>| {
        let mut pose = Pose::from_identity(skeleton.len());
        let mut ctx = EvalContext::new(
            &skeleton,
            &clips,
            &params,
            1.0 / 30.0,
            Entity::PLACEHOLDER,
            &sockets,
            scratch,
        )
        .with_reuse(std::mem::take(&mut reuse));
        evaluate_graph(&graph, &mut instance, &mut ctx, &mut pose).expect("evaluate");
        reuse = ctx.take_reuse();
        ctx.finish().scratch
    };
    let scratch = step(Vec::new());
    let names = joint_index_calls();
    let fills = globals_fills();
    let _scratch = step(scratch);
    let name_delta = joint_index_calls() - names;
    let fill_delta = globals_fills() - fills;
    assert_eq!(
        name_delta, 0,
        "the second step scanned joint names {name_delta} times"
    );
    assert_eq!(
        fill_delta, 0,
        "the second step allocated {fill_delta} global buffers"
    );
}

/// Audit (corr-animation): the cached chain was keyed by the node's and skeleton's addresses
/// alone, so a node edited in place (or a graph reloaded at a freed address) kept solving the
/// old chain.
#[test]
fn an_edited_chain_at_the_same_address_is_resolved_again() {
    // integ-v048c: this test scans joint names; without the counter lock it ran beside
    // `the_second_two_bone_step_does_not_rescan_or_refill_globals` and moved that test's
    // process-wide counters ("the second step scanned joint names 3 times").
    let _guard = hot_count::lock_for_test();
    let skeleton = arm(40);
    let mut graph = AnimGraph {
        parameters: vec![ParamDecl(
            "Hand".into(),
            ParamValue::Vec3(Vec3::new(0.0, 0.3, 0.2)),
        )],
        root: AnimNode::TwoBoneIK(TwoBoneIKNode {
            inner: Box::new(AnimNode::Clip(ClipNode {
                clip: "idle".into(),
                looping: true,
                speed: 1.0,
                sync_group: None,
            })),
            chain: ["j0".into(), "j1".into(), "j2".into()],
            target: IkTarget::Param("Hand".into()),
            pole: None,
            weight_param: None,
        }),
    };
    let mut clips = ClipLibrary::new();
    clips.insert(AnimationClip::empty("idle", 1.0));
    let params = graph.default_parameters();
    let mut instance = GraphInstance::from_node(&graph.root);
    let sockets = HashMap::new();
    let mut reuse = EvalReuse::default();
    let mut pose = Pose::from_identity(skeleton.len());
    for edit in [false, true] {
        if edit && let AnimNode::TwoBoneIK(node) = &mut graph.root {
            node.chain = ["j3".into(), "j4".into(), "j5".into()];
        }
        let mut ctx = EvalContext::new(
            &skeleton,
            &clips,
            &params,
            1.0 / 30.0,
            Entity::PLACEHOLDER,
            &sockets,
            Vec::new(),
        )
        .with_reuse(std::mem::take(&mut reuse));
        evaluate_graph(&graph, &mut instance, &mut ctx, &mut pose).expect("evaluate");
        reuse = ctx.take_reuse();
        ctx.finish();
    }
    let identity = regia_core::math::Quat::IDENTITY;
    assert!(
        pose.rotations[0].abs_diff_eq(identity, 1e-6),
        "the old chain moved: {:?}",
        pose.rotations[0]
    );
    assert!(
        !pose.rotations[3].abs_diff_eq(identity, 1e-3),
        "the new chain did not move"
    );
}
