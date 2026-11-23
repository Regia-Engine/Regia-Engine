// SPDX-License-Identifier: MPL-2.0
//! IK residual capture behind the rig's debug flag (`anim.ik_report`, ADR-8708).
//!
//! One graph nests the three IK node kinds the graph authors use for locomotion and aiming:
//! `TwoBoneIK` (an arm), `LookAt` (the head) and `ChainIK` (a six-joint tail). With the flag
//! off the evaluator measures nothing (a counter proves it) and the debug snapshot has no IK
//! rows; with it on every solved node reports its residual, iterations, clamp flag and weight
//! under its graph path, and the flag never changes the pose.

use std::collections::HashMap;

use regia_animation::Skeleton;
use regia_animation::chain_ik::ChainSolver;
use regia_animation::clip::AnimationClip;
use regia_animation::debug::{IkNodeKind, graph_debug_with_ik};
use regia_animation::eval::{ClipLibrary, EvalContext, EvalReuse, GraphInstance, evaluate_graph};
use regia_animation::graph::{
    AnimGraph, AnimNode, ChainIKNode, ClipNode, IkTarget, LookAtNode, TwoBoneIKNode,
};
use regia_animation::params::{ParamDecl, ParamValue, ParameterSet};
use regia_animation::pose::Pose;
use regia_animation::skeleton::LocalBind;
use regia_core::math::Vec3;
use regia_ecs::prelude::Entity;

/// `hips` at 1 m; an arm `shoulder -> elbow -> hand` (0.3 m + 0.3 m along +X); a `head` 0.5 m
/// above the hips; a tail `t0..t5` (0.1 m, then 0.2 m segments along −Y, 1.1 m in all).
fn rig() -> Skeleton {
    let mut names = Vec::new();
    let mut parents = Vec::new();
    let mut binds = Vec::new();
    let mut push = |name: &str, parent: Option<u32>, t: Vec3| {
        names.push(name.to_string());
        parents.push(parent);
        binds.push(LocalBind {
            translation: t,
            ..LocalBind::default()
        });
    };
    push("hips", None, Vec3::new(0.0, 1.0, 0.0));
    push("shoulder", Some(0), Vec3::new(0.2, 0.3, 0.0));
    push("elbow", Some(1), Vec3::new(0.3, 0.0, 0.0));
    push("hand", Some(2), Vec3::new(0.3, 0.0, 0.0));
    push("head", Some(0), Vec3::new(0.0, 0.5, 0.0));
    for i in 0..6u32 {
        let parent = if i == 0 { 0 } else { 4 + i };
        let length = if i == 0 { 0.1 } else { 0.2 };
        push(&format!("t{i}"), Some(parent), Vec3::new(0.0, -length, 0.0));
    }
    Skeleton::from_local_bind(names, parents, binds).expect("ik report rig")
}

fn graph() -> AnimGraph {
    let clip = AnimNode::Clip(ClipNode {
        clip: "idle".into(),
        looping: true,
        speed: 1.0,
        sync_group: None,
    });
    let chain = AnimNode::ChainIK(ChainIKNode {
        inner: Box::new(clip),
        root: "t0".into(),
        tip: "t5".into(),
        solver: ChainSolver::Fabrik,
        target: IkTarget::Param("Tail".into()),
        pole: None,
        limits: None,
        iterations: 128,
        tolerance: 1e-5,
        weight_param: None,
    });
    let look = AnimNode::LookAt(LookAtNode {
        inner: Box::new(chain),
        joint: "head".into(),
        target: IkTarget::Param("Look".into()),
        max_yaw: 1.2,
        max_pitch: 0.8,
        weight_param: None,
    });
    AnimGraph {
        parameters: vec![
            ParamDecl("Hand".into(), ParamValue::Vec3(Vec3::ZERO)),
            ParamDecl("Look".into(), ParamValue::Vec3(Vec3::ZERO)),
            ParamDecl("Tail".into(), ParamValue::Vec3(Vec3::ZERO)),
        ],
        root: AnimNode::TwoBoneIK(TwoBoneIKNode {
            inner: Box::new(look),
            chain: ["shoulder".into(), "elbow".into(), "hand".into()],
            target: IkTarget::Param("Hand".into()),
            pole: None,
            weight_param: None,
        }),
    }
}

struct Runner {
    skeleton: Skeleton,
    graph: AnimGraph,
    clips: ClipLibrary,
    instance: GraphInstance,
    reuse: EvalReuse,
    scratch: Vec<Pose>,
    sockets: HashMap<(Entity, String), Vec3>,
    pose: Pose,
}

impl Runner {
    fn new() -> Self {
        let skeleton = rig();
        let graph = graph();
        let mut clips = ClipLibrary::new();
        clips.insert(AnimationClip::empty("idle", 1.0));
        let instance = GraphInstance::from_node(&graph.root);
        let pose = Pose::from_identity(skeleton.len());
        Runner {
            skeleton,
            graph,
            clips,
            instance,
            reuse: EvalReuse::default(),
            scratch: Vec::new(),
            sockets: HashMap::new(),
            pose,
        }
    }

    fn step(&mut self, params: &ParameterSet) {
        let mut ctx = EvalContext::new(
            &self.skeleton,
            &self.clips,
            params,
            1.0 / 60.0,
            Entity::PLACEHOLDER,
            &self.sockets,
            std::mem::take(&mut self.scratch),
        )
        .with_reuse(std::mem::take(&mut self.reuse));
        evaluate_graph(&self.graph, &mut self.instance, &mut ctx, &mut self.pose).expect("evaluate");
        self.reuse = ctx.take_reuse();
        self.scratch = ctx.finish().scratch;
    }
}

/// Reachable targets moving a little each frame: hand inside the arm's annulus, a gaze inside
/// the head's limits, a tail tip inside the tail's reach.
fn params(frame: usize) -> ParameterSet {
    let mut params = graph().default_parameters();
    let t = frame as f32 * 0.05;
    params.set_vec3(
        "Hand",
        Vec3::new(0.55 + 0.03 * t.sin(), 1.15 + 0.05 * t.cos(), 0.1),
    );
    params.set_vec3("Look", Vec3::new(0.6 * t.sin(), 1.6, -2.0));
    params.set_vec3("Tail", Vec3::new(0.3 + 0.1 * t.cos(), 0.35, 0.2 * t.sin()));
    params
}

fn pose_bits(pose: &Pose) -> Vec<u32> {
    pose.translations
        .iter()
        .chain(&pose.scales)
        .flat_map(|v| v.to_array())
        .chain(pose.rotations.iter().flat_map(|q| q.to_array()))
        .map(f32::to_bits)
        .collect()
}

/// Off (the default): 120 frames measure nothing; the counter stays 0 and the snapshot has no
/// IK rows. The pose is the same, bit for bit, as with the flag on.
#[test]
fn ik_report_off_costs_nothing() {
    let mut off = Runner::new();
    let mut on = Runner::new();
    on.reuse.set_ik_debug(true);
    for frame in 0..120 {
        let p = params(frame);
        off.step(&p);
        on.step(&p);
        assert_eq!(
            pose_bits(&off.pose),
            pose_bits(&on.pose),
            "the debug flag changed the pose at frame {frame}"
        );
    }
    assert!(!off.reuse.ik_debug());
    assert_eq!(
        off.reuse.ik_debug_captures(),
        0,
        "the off path measured residuals"
    );
    let snapshot = graph_debug_with_ik(&off.graph, &off.instance, &params(119), &off.reuse);
    assert!(snapshot.ik.is_empty(), "{:?}", snapshot.ik);
    // On: three nodes, one capture each per frame.
    assert_eq!(on.reuse.ik_debug_captures(), 3 * 120);
}

/// On: every node solved, reachable targets come within 1 mm (1 mrad for the gaze), paths
/// match the graph snapshot's, and an out-of-reach hand reports `clamped`.
#[test]
fn ik_report_on_reports_residual_under_1mm() {
    let mut runner = Runner::new();
    runner.reuse.set_ik_debug(true);
    for frame in 0..60 {
        let p = params(frame);
        runner.step(&p);
        let snapshot = graph_debug_with_ik(&runner.graph, &runner.instance, &p, &runner.reuse);
        let kinds: Vec<(&str, IkNodeKind)> = snapshot.ik.iter().map(|r| (r.path.as_str(), r.kind)).collect();
        assert_eq!(
            kinds,
            [
                ("root", IkNodeKind::TwoBoneIk),
                ("root/inner", IkNodeKind::LookAt),
                ("root/inner/inner", IkNodeKind::ChainIk),
            ]
        );
        for row in &snapshot.ik {
            assert!(
                snapshot.node(&row.path).is_some(),
                "{} is not a graph node path",
                row.path
            );
            assert!(row.solved, "{row:?}");
            assert!(!row.clamped, "frame {frame}: {row:?}");
            assert_eq!(row.weight, 1.0);
            assert!(row.residual < 1.0e-3, "frame {frame}: {row:?}");
            assert!(row.iterations >= 1, "{row:?}");
        }
    }
    // A hand target 2 m away is out of the 0.6 m arm's reach.
    let mut far = params(0);
    far.set_vec3("Hand", Vec3::new(2.5, 1.3, 0.0));
    runner.step(&far);
    let snapshot = graph_debug_with_ik(&runner.graph, &runner.instance, &far, &runner.reuse);
    let hand = &snapshot.ik[0];
    assert!(hand.clamped, "{hand:?}");
    assert!((hand.residual - (2.5_f32 - 0.2 - 0.6)).abs() < 0.01, "{hand:?}");
    // Turned off: rows disappear at once and the counter stops.
    let before = runner.reuse.ik_debug_captures();
    runner.reuse.set_ik_debug(false);
    runner.step(&params(1));
    assert_eq!(runner.reuse.ik_debug_captures(), before);
    let snapshot = graph_debug_with_ik(&runner.graph, &runner.instance, &params(1), &runner.reuse);
    assert!(snapshot.ik.is_empty());
}
