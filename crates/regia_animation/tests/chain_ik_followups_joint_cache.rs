// SPDX-License-Identifier: MPL-2.0
//! Lane anim-chain-ik-followups (ADR-8735, review-c finding 4 applied to the older IK caches):
//! the `TwoBoneIK` and `LookAt` joint caches held 64 slots each and were cleared when full, so
//! 65 such nodes on one rig scanned joint names for every node every frame. They now keep live
//! slots and replace stale ones like the `ChainIK`/`BodyAim` cache. Counted with the crate's
//! hot-path counters (one test per binary: the counters are process-wide).

use std::collections::HashMap;

use regia_animation::Skeleton;
use regia_animation::clip::AnimationClip;
use regia_animation::eval::{ClipLibrary, EvalContext, EvalReuse, GraphInstance, evaluate_graph};
use regia_animation::graph::{AnimGraph, AnimNode, ClipNode, IkTarget, LookAtNode, TwoBoneIKNode};
use regia_animation::hot_count::{self, joint_index_calls};
use regia_animation::params::{ParamDecl, ParamValue};
use regia_animation::pose::Pose;
use regia_animation::skeleton::LocalBind;
use regia_core::math::Vec3;
use regia_ecs::prelude::Entity;

fn arm() -> Skeleton {
    let names = (0..5).map(|i| format!("j{i}")).collect();
    let parents = (0..5u32).map(|i| i.checked_sub(1)).collect();
    let binds = (0..5)
        .map(|_| LocalBind {
            translation: Vec3::new(0.0, 0.2, 0.0),
            ..LocalBind::default()
        })
        .collect();
    Skeleton::from_local_bind(names, parents, binds).expect("skeleton")
}

/// `n` nested `TwoBoneIK` nodes and `n` nested `LookAt` nodes over a clip.
fn graph(n: usize) -> AnimGraph {
    let mut root = Box::new(AnimNode::Clip(ClipNode {
        clip: "idle".into(),
        looping: true,
        speed: 1.0,
        sync_group: None,
    }));
    for _ in 0..n {
        root = Box::new(AnimNode::TwoBoneIK(TwoBoneIKNode {
            inner: root,
            chain: ["j0".into(), "j1".into(), "j2".into()],
            target: IkTarget::Param("Hand".into()),
            pole: None,
            weight_param: None,
        }));
        root = Box::new(AnimNode::LookAt(LookAtNode {
            inner: root,
            joint: "j4".into(),
            target: IkTarget::Param("Hand".into()),
            max_yaw: 1.0,
            max_pitch: 1.0,
            weight_param: None,
        }));
    }
    AnimGraph {
        parameters: vec![ParamDecl(
            "Hand".into(),
            ParamValue::Vec3(Vec3::new(0.0, 0.3, 0.2)),
        )],
        root: *root,
    }
}

/// 65 two-bone and 65 look-at nodes (one more of each than the old caches held), and the same
/// graph after a reload: once warm, no evaluation scans joint names. Fails before the fix.
#[test]
fn sixty_five_two_bone_and_look_at_nodes_resolve_once() {
    let _guard = hot_count::lock_for_test();
    let skeleton = arm();
    let mut clips = ClipLibrary::new();
    clips.insert(AnimationClip::empty("idle", 1.0));
    let sockets = HashMap::new();
    let mut reuse = EvalReuse::default();
    let mut scratch = Vec::new();
    let mut pose = Pose::from_identity(skeleton.len());
    let mut step = |graph: &AnimGraph, instance: &mut GraphInstance| {
        let params = graph.default_parameters();
        let mut ctx = EvalContext::new(
            &skeleton,
            &clips,
            &params,
            1.0 / 60.0,
            Entity::PLACEHOLDER,
            &sockets,
            std::mem::take(&mut scratch),
        )
        .with_reuse(std::mem::take(&mut reuse));
        evaluate_graph(graph, instance, &mut ctx, &mut pose).expect("evaluate");
        reuse = ctx.take_reuse();
        scratch = ctx.finish().scratch;
    };
    let old = graph(65);
    let new = old.clone();
    for (what, g) in [("first load", &old), ("after a reload", &new)] {
        let mut instance = GraphInstance::from_node(&g.root);
        for _ in 0..8 {
            step(g, &mut instance);
        }
        let before = joint_index_calls();
        for _ in 0..10 {
            step(g, &mut instance);
        }
        let scans = joint_index_calls() - before;
        assert_eq!(
            scans, 0,
            "{what}: 10 warm frames scanned joint names {scans} times"
        );
    }
}
