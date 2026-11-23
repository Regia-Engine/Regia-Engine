// SPDX-License-Identifier: MPL-2.0
//! Lane anim-chain-ik-followups, review-c finding 4 (ADR-8735): the rig's `ChainIK` /
//! `BodyAim` slot cache must not thrash. Before the fix it held 64 slots and was cleared when
//! full, so a graph with more IK nodes than that, or two graphs evaluated in turn on one rig,
//! re-resolved every chain every frame (a `JointChain` `Vec`, cloned names and limits per node).
//! A warm rig must allocate no more than the same rig with no IK nodes. These tests count
//! allocations; they never time anything.

use std::collections::HashMap;

use regia_animation::Skeleton;
use regia_animation::chain_ik::{ChainSolver, JointLimit};
use regia_animation::clip::AnimationClip;
use regia_animation::eval::{ClipLibrary, EvalContext, EvalReuse, GraphInstance, evaluate_graph};
use regia_animation::graph::{
    AnimGraph, AnimNode, BodyAimNode, ChainIKNode, ChainLimits, ClipNode, IkTarget,
};
use regia_animation::params::{ParamDecl, ParamValue, ParameterSet};
use regia_animation::pose::Pose;
use regia_animation::skeleton::LocalBind;
use regia_core::math::Vec3;
use regia_ecs::prelude::Entity;
use regia_test_utils::alloc_counter::{self, CountingAlloc};

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc::new();

/// `hips`, a tail `t0..t5` and a neck `n0..n2` with a `head`.
fn rig() -> Skeleton {
    let mut names = vec!["hips".to_string()];
    let mut parents = vec![None];
    let mut binds = vec![Vec3::new(0.0, 0.5, 0.0)];
    for i in 0..6u32 {
        names.push(format!("t{i}"));
        parents.push(Some(i));
        binds.push(Vec3::new(0.0, 0.2, 0.0));
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

fn clip() -> Box<AnimNode> {
    Box::new(AnimNode::Clip(ClipNode {
        clip: "idle".into(),
        looping: true,
        speed: 1.0,
        sync_group: None,
    }))
}

fn params() -> Vec<ParamDecl> {
    vec![
        ParamDecl("Tail".into(), ParamValue::Vec3(Vec3::new(0.3, 1.2, 0.3))),
        ParamDecl("Look".into(), ParamValue::Vec3(Vec3::new(1.0, 1.5, -6.0))),
    ]
}

fn chain(inner: Box<AnimNode>) -> AnimNode {
    AnimNode::ChainIK(ChainIKNode {
        inner,
        root: "t0".into(),
        tip: "t5".into(),
        solver: ChainSolver::Fabrik,
        target: IkTarget::Param("Tail".into()),
        pole: None,
        limits: Some(ChainLimits::Uniform(JointLimit::Cone {
            max_swing: 0.6,
            min_twist: -0.2,
            max_twist: 0.2,
        })),
        iterations: 4,
        tolerance: 1e-3,
        weight_param: None,
    })
}

fn aim(inner: Box<AnimNode>) -> AnimNode {
    AnimNode::BodyAim(BodyAimNode {
        inner,
        joints: vec![("n0".into(), 1.0), ("n1".into(), 1.0), ("n2".into(), 2.0)],
        aim_joint: "head".into(),
        axis: Vec3::NEG_Z,
        target: IkTarget::Param("Look".into()),
        limits: None,
        passes: 2,
        weight_param: None,
    })
}

/// A graph of `chains` nested `ChainIK` nodes and `aims` nested `BodyAim` nodes over a clip.
fn graph(chains: usize, aims: usize) -> AnimGraph {
    let mut root = clip();
    for _ in 0..chains {
        root = Box::new(chain(root));
    }
    for _ in 0..aims {
        root = Box::new(aim(root));
    }
    let graph = AnimGraph {
        parameters: params(),
        root: *root,
    };
    graph.validate().expect("valid");
    graph
}

/// One rig: kept reuse buffers, evaluated with whatever graph each step names.
struct Rig {
    skeleton: Skeleton,
    clips: ClipLibrary,
    reuse: EvalReuse,
    scratch: Vec<Pose>,
    sockets: HashMap<(Entity, String), Vec3>,
    pose: Pose,
    params: ParameterSet,
}

impl Rig {
    fn new() -> Self {
        let skeleton = rig();
        let mut clips = ClipLibrary::new();
        clips.insert(AnimationClip::empty("idle", 1.0));
        let pose = Pose::from_identity(skeleton.len());
        Rig {
            params: graph(0, 0).default_parameters(),
            skeleton,
            clips,
            reuse: EvalReuse::default(),
            scratch: Vec::new(),
            sockets: HashMap::new(),
            pose,
        }
    }

    fn step(&mut self, graph: &AnimGraph, instance: &mut GraphInstance) {
        let mut ctx = EvalContext::new(
            &self.skeleton,
            &self.clips,
            &self.params,
            1.0 / 60.0,
            Entity::PLACEHOLDER,
            &self.sockets,
            std::mem::take(&mut self.scratch),
        )
        .with_reuse(std::mem::take(&mut self.reuse));
        evaluate_graph(graph, instance, &mut ctx, &mut self.pose).expect("evaluate");
        self.reuse = ctx.take_reuse();
        self.scratch = ctx.finish().scratch;
    }
}

/// Allocations of `frames` warm frames of a rig that plays `graphs` in turn, one per frame.
fn warm_allocations(graphs: &[&AnimGraph], warm: usize, frames: usize) -> u64 {
    let mut rig = Rig::new();
    let mut instances: Vec<GraphInstance> =
        graphs.iter().map(|g| GraphInstance::from_node(&g.root)).collect();
    let mut frame = 0usize;
    for _ in 0..warm {
        let k = frame % graphs.len();
        rig.step(graphs[k], &mut instances[k]);
        frame += 1;
    }
    let ((), stats) = alloc_counter::measure(|| {
        for _ in 0..frames {
            let k = frame % graphs.len();
            rig.step(graphs[k], &mut instances[k]);
            frame += 1;
        }
    });
    stats.allocs
}

/// The same rig with no IK nodes: what a warm IK rig may allocate at most.
fn plain(frames: usize) -> u64 {
    let g = graph(0, 0);
    warm_allocations(&[&g], 8, frames)
}

/// 65 `ChainIK` and 65 `BodyAim` nodes (one more than the old cache held of each) warm up and
/// then allocate no more than the plain rig. Fails before the fix (the cache cleared itself
/// every frame).
#[test]
fn sixty_five_chain_and_aim_nodes_stay_allocation_free_when_warm() {
    let g = graph(65, 65);
    let with_nodes = warm_allocations(&[&g], 8, 20);
    assert_eq!(
        with_nodes,
        plain(20),
        "65 chain + 65 aim nodes allocated {} more over 20 warm frames",
        with_nodes as i64 - plain(20) as i64
    );
}

/// A graph reload (the editor recompiles, an asset hot-reloads): the new graph's nodes live at
/// new addresses, the old graph's slots go stale. With 65 + 65 nodes the reloaded graph warms up
/// within a few frames and is then allocation-free again; before the fix it never warmed.
#[test]
fn sixty_five_nodes_after_a_reload_stay_allocation_free_when_warm() {
    let old = graph(65, 65);
    let new = old.clone();
    let mut rig = Rig::new();
    let mut instance = GraphInstance::from_node(&old.root);
    for _ in 0..8 {
        rig.step(&old, &mut instance);
    }
    // Reload: the old graph stays alive, so its addresses cannot be reused by the new one.
    let mut instance = GraphInstance::from_node(&new.root);
    for _ in 0..8 {
        rig.step(&new, &mut instance);
    }
    let ((), stats) = alloc_counter::measure(|| {
        for _ in 0..20 {
            rig.step(&new, &mut instance);
        }
    });
    let base = plain(20);
    assert_eq!(
        stats.allocs,
        base,
        "the reloaded 65 + 65 node graph allocated {} more over 20 warm frames",
        stats.allocs as i64 - base as i64
    );
    drop(old);
}

/// Repeated reloads of a 40 + 40 node graph (within the old cache size): every reload warms up
/// again and the rig is allocation-free between reloads; the stale slots of the earlier copies
/// never make the cache thrash.
#[test]
fn repeated_reloads_warm_up_again() {
    let first = graph(40, 40);
    let copies: Vec<AnimGraph> = (0..6).map(|_| first.clone()).collect();
    let mut rig = Rig::new();
    let base = plain(10);
    for (n, g) in copies.iter().enumerate() {
        let mut instance = GraphInstance::from_node(&g.root);
        for _ in 0..8 {
            rig.step(g, &mut instance);
        }
        let ((), stats) = alloc_counter::measure(|| {
            for _ in 0..10 {
                rig.step(g, &mut instance);
            }
        });
        assert_eq!(
            stats.allocs,
            base,
            "reload {n}: {} extra allocations",
            stats.allocs as i64 - base as i64
        );
    }
}

/// Two 40 + 40 node graphs evaluated in turn on one rig (80 + 80 live slots, more than the old
/// cache held): neither evicts the other's slots. Fails before the fix (each evaluation cleared
/// the other graph's chains).
#[test]
fn two_graphs_in_turn_on_one_rig_stay_allocation_free_when_warm() {
    let a = graph(40, 40);
    let b = graph(40, 40);
    let with_nodes = warm_allocations(&[&a, &b], 8, 20);
    let base = plain(20);
    assert_eq!(
        with_nodes,
        base,
        "two graphs in turn allocated {} more over 20 warm frames",
        with_nodes as i64 - base as i64
    );
}
